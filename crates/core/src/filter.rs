use crate::MidiEvent;

/// Which incoming MIDI a rack slot responds to: a keyboard split/layer zone.
///
/// The note and velocity ranges are inclusive. A range whose low bound is above its
/// high bound matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiFilter {
    /// 0-based MIDI channel to accept, or `None` to accept every channel (omni).
    pub channel: Option<u8>,
    pub note_low: u8,
    pub note_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
}

impl Default for MidiFilter {
    /// Lets everything through.
    fn default() -> Self {
        Self {
            channel: None,
            note_low: 0,
            note_high: 127,
            velocity_low: 1,
            velocity_high: 127,
        }
    }
}

/// Applies a [`MidiFilter`] to a stream of events for one slot, while tracking which
/// notes it has let through.
///
/// Note-offs are routed by that tracking, not by the filter: a note-off passes if and
/// only if this router passed the matching note-on. So changing the filter while
/// keys are held never leaves a note stuck, and a note-off for a key that went to a
/// different slot (same note number, other channel/zone) never cuts this slot's
/// voice short. Fixed-size and allocation-free, so it runs on the audio thread.
#[derive(Debug, Clone)]
pub struct MidiRouter {
    filter: MidiFilter,
    /// One bit per (channel, note) currently held in this slot.
    held: [u128; 16],
}

impl MidiRouter {
    pub fn new(filter: MidiFilter) -> Self {
        Self {
            filter,
            held: [0; 16],
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

    /// Whether `event` should be passed to this slot's plugin.
    pub fn accept(&mut self, event: &MidiEvent) -> bool {
        let bytes = event.as_slice();
        let status = bytes[0];
        if status >= 0xF0 {
            // System messages (clock, start/stop, ...) aren't channel-specific.
            return true;
        }
        let channel = status & 0x0F;
        let kind = status & 0xF0;
        let note = bytes.get(1).copied().unwrap_or(0) & 0x7F;
        let velocity = bytes.get(2).copied().unwrap_or(0);
        let bit = 1u128 << note;
        let held = &mut self.held[usize::from(channel)];

        let is_note_off = kind == 0x80 || (kind == 0x90 && velocity == 0);
        if is_note_off {
            let was_held = *held & bit != 0;
            *held &= !bit;
            return was_held;
        }

        let filter = &self.filter;
        if filter.channel.is_some_and(|wanted| wanted != channel) {
            return false;
        }
        let note_in_range = (filter.note_low..=filter.note_high).contains(&note);
        match kind {
            0x90 => {
                let accepted = note_in_range
                    && (filter.velocity_low..=filter.velocity_high).contains(&velocity);
                if accepted {
                    *held |= bit;
                }
                accepted
            }
            // Polyphonic aftertouch belongs to a specific key.
            0xA0 => *held & bit != 0,
            // CCs (sustain, mod wheel, ...), program change, channel pressure, pitch
            // bend: not tied to a key, so only the channel filter applies.
            _ => true,
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
        assert!(router.accept(&ev(&[0x90, 60, 10])));
        assert!(router.accept(&ev(&[0x90, 72, 100])));
        assert!(!router.accept(&ev(&[0x90, 59, 64])));
        assert!(!router.accept(&ev(&[0x90, 73, 64])));
        assert!(!router.accept(&ev(&[0x90, 65, 9])));
        assert!(!router.accept(&ev(&[0x90, 65, 101])));
    }

    #[test]
    fn channel_filter_applies_to_non_note_messages() {
        let mut router = MidiRouter::new(MidiFilter {
            channel: Some(1),
            ..Default::default()
        });
        assert!(router.accept(&ev(&[0xB1, 64, 127])));
        assert!(!router.accept(&ev(&[0xB0, 64, 127])));
        assert!(!router.accept(&ev(&[0x90, 60, 64])));
        assert!(router.accept(&ev(&[0xF8])));
    }

    #[test]
    fn note_off_follows_its_note_on_not_the_current_filter() {
        let mut router = MidiRouter::new(split(60, 72));
        assert!(router.accept(&ev(&[0x90, 60, 64])));
        // Range moved away while the key is held: its release must still arrive.
        router.set_filter(split(0, 10));
        assert!(router.accept(&ev(&[0x80, 60, 0])));
        // Only once, and never for a note this slot didn't play.
        assert!(!router.accept(&ev(&[0x80, 60, 0])));
        assert!(!router.accept(&ev(&[0x90, 61, 0])));
    }

    #[test]
    fn note_off_on_another_channel_does_not_release() {
        let mut router = MidiRouter::new(MidiFilter {
            channel: Some(0),
            ..Default::default()
        });
        assert!(router.accept(&ev(&[0x90, 60, 64])));
        assert!(!router.accept(&ev(&[0x81, 60, 0])));
        assert!(router.accept(&ev(&[0x80, 60, 0])));
    }

    #[test]
    fn inverted_range_matches_nothing() {
        let mut router = MidiRouter::new(split(72, 60));
        assert!(!router.accept(&ev(&[0x90, 65, 64])));
    }
}
