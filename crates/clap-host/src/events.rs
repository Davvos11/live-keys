//! Fixed-capacity CLAP event lists for `process`, allocated at load time.

use std::ffi::c_void;

use clap_sys::events::*;
use live_keys_core::MidiEvent;

/// Room for any one event this host sends.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union InputEvent {
    header: clap_event_header,
    note: clap_event_note,
    midi: clap_event_midi,
    param: clap_event_param_value,
}

fn header<T>(event_type: u16, time: u32) -> clap_event_header {
    clap_event_header {
        size: std::mem::size_of::<T>() as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_: event_type,
        flags: CLAP_EVENT_IS_LIVE,
    }
}

/// A block's input events, in time order (parameter changes, all at 0, first).
pub(crate) struct InputEvents {
    events: Vec<InputEvent>,
}

impl InputEvents {
    pub fn new(capacity: usize) -> Self {
        Self { events: Vec::with_capacity(capacity) }
    }

    fn push(&mut self, event: InputEvent) {
        // Never grows: a burst beyond capacity is dropped.
        if self.events.len() < self.events.capacity() {
            self.events.push(event);
        }
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn push_param(&mut self, param_id: u32, value: f64) {
        self.push(InputEvent {
            param: clap_event_param_value {
                header: header::<clap_event_param_value>(CLAP_EVENT_PARAM_VALUE, 0),
                param_id,
                cookie: std::ptr::null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value,
            },
        });
    }

    /// `event` as a CLAP note event where it is one and `clap_notes` is set, else
    /// as a raw MIDI event if `raw_midi` is set, at `time` (within the block).
    pub fn push_midi(&mut self, event: &MidiEvent, time: u32, clap_notes: bool, raw_midi: bool) {
        let bytes = event.as_slice();
        let Some(&status) = bytes.first() else { return };
        let (channel, key) = (i16::from(status & 0x0F), i16::from(bytes.get(1).copied().unwrap_or(0) & 0x7F));
        let velocity = f64::from(bytes.get(2).copied().unwrap_or(0) & 0x7F) / 127.0;
        let note = |event_type| InputEvent {
            note: clap_event_note {
                header: header::<clap_event_note>(event_type, time),
                note_id: -1,
                port_index: 0,
                channel,
                key,
                velocity,
            },
        };
        match status & 0xF0 {
            0x90 if clap_notes && velocity > 0.0 => self.push(note(CLAP_EVENT_NOTE_ON)),
            0x80 | 0x90 if clap_notes => self.push(note(CLAP_EVENT_NOTE_OFF)),
            _ if raw_midi => {
                let mut data = [0; 3];
                data[..bytes.len().min(3)].copy_from_slice(&bytes[..bytes.len().min(3)]);
                self.push(InputEvent {
                    midi: clap_event_midi {
                        header: header::<clap_event_midi>(CLAP_EVENT_MIDI, time),
                        port_index: 0,
                        data,
                    },
                });
            }
            _ => {}
        }
    }

    /// The `clap_input_events` the plugin reads this list through. Valid while
    /// `self` isn't moved or changed.
    pub fn as_clap(&self) -> clap_input_events {
        clap_input_events {
            ctx: self as *const Self as *mut c_void,
            size: Some(input_size),
            get: Some(input_get),
        }
    }
}

unsafe extern "C" fn input_size(list: *const clap_input_events) -> u32 {
    let events = unsafe { &*((*list).ctx as *const InputEvents) };
    events.events.len() as u32
}

unsafe extern "C" fn input_get(list: *const clap_input_events, index: u32) -> *const clap_event_header {
    let events = unsafe { &*((*list).ctx as *const InputEvents) };
    match events.events.get(index as usize) {
        Some(event) => unsafe { &event.header },
        None => std::ptr::null(),
    }
}

/// What a block's output events are turned into: MIDI for the next chain stage,
/// and parameter values the plugin changed itself (its GUI's edits).
pub(crate) struct OutputEvents {
    pub midi: Vec<MidiEvent>,
    pub params: Vec<(u32, f64)>,
}

impl OutputEvents {
    pub fn new(capacity: usize) -> Self {
        Self {
            midi: Vec::with_capacity(capacity),
            params: Vec::with_capacity(capacity),
        }
    }

    pub fn clear(&mut self) {
        self.midi.clear();
        self.params.clear();
    }

    pub fn as_clap(&mut self) -> clap_output_events {
        clap_output_events {
            ctx: self as *mut Self as *mut c_void,
            try_push: Some(output_try_push),
        }
    }
}

unsafe extern "C" fn output_try_push(list: *const clap_output_events, event: *const clap_event_header) -> bool {
    let (Some(list), Some(header)) = (unsafe { list.as_ref() }, unsafe { event.as_ref() }) else {
        return false;
    };
    let out = unsafe { &mut *(list.ctx as *mut OutputEvents) };
    if header.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return true;
    }
    let time = header.time;
    let midi = match header.type_ {
        CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF => {
            let note = unsafe { &*(event as *const clap_event_note) };
            if !(0..16).contains(&note.channel) || !(0..128).contains(&note.key) {
                return true;
            }
            let velocity = (note.velocity * 127.0).round().clamp(0.0, 127.0) as u8;
            let (status, velocity) = if header.type_ == CLAP_EVENT_NOTE_ON {
                // Velocity 0 would read as a note-off.
                (0x90, velocity.max(1))
            } else {
                (0x80, velocity)
            };
            MidiEvent::from_raw(time, &[status | note.channel as u8, note.key as u8, velocity])
        }
        CLAP_EVENT_MIDI => {
            let midi = unsafe { &*(event as *const clap_event_midi) };
            let length = match midi.data[0] & 0xF0 {
                0xC0 | 0xD0 => 2,
                _ => 3,
            };
            MidiEvent::from_raw(time, &midi.data[..length])
        }
        CLAP_EVENT_PARAM_VALUE => {
            let param = unsafe { &*(event as *const clap_event_param_value) };
            if out.params.len() < out.params.capacity() {
                out.params.push((param.param_id, param.value));
            }
            return true;
        }
        _ => return true,
    };
    match midi {
        Some(midi) if out.midi.len() < out.midi.capacity() => {
            out.midi.push(midi);
            true
        }
        Some(_) => false,
        None => true,
    }
}
