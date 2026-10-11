//! The audio-thread side of the rack: each slot's chain of plugins, the commands
//! that change it, and rendering one callback's worth of audio. Nothing in here
//! allocates after construction.

use live_keys_core::{
    HostedPlugin, MidiConsumer, MidiEvent, MidiFilter, MidiRouter, ParamChange, Producer,
    PushError,
};

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::engine::{SLOT_COUNT, SlotSettings};

/// Each slot's output peak (absolute sample value, after its gain, as `f32`
/// bits) since the control thread last took it — a meter. Non-negative floats
/// order like their bits, so `fetch_max` on the bits keeps the larger one.
pub(crate) type SlotPeaks = Arc<[AtomicU32; SLOT_COUNT]>;

/// Most audio effects one slot's chain can hold after its instrument.
pub const MAX_EFFECTS: usize = 8;
/// Most MIDI effects one slot's chain can hold before its instrument.
pub const MAX_MIDI_EFFECTS: usize = 8;
/// Instrument + audio effects.
const CHAIN_LEN: usize = 1 + MAX_EFFECTS;
/// CC 123 (all notes off) on every channel, sent to an instrument whenever the
/// MIDI effects in front of it change: notes they started (or transposed) would
/// otherwise never get their note-off.
const ALL_NOTES_OFF: [[u8; 3]; 16] = {
    let mut events = [[0xB0, 123, 0]; 16];
    let mut channel = 0;
    while channel < 16 {
        events[channel][0] |= channel as u8;
        channel += 1;
    }
    events
};

pub(crate) type BoxedPlugin = Box<dyn HostedPlugin>;

/// The two effect chains of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// MIDI effects, in front of the instrument.
    Midi,
    /// Audio effects, after the instrument.
    Audio,
}

impl Section {
    pub fn capacity(self) -> usize {
        match self {
            Section::Midi => MAX_MIDI_EFFECTS,
            Section::Audio => MAX_EFFECTS,
        }
    }
}

/// Where a plugin sits in a slot's chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainPosition {
    /// The nth MIDI effect in front of the instrument.
    MidiEffect(usize),
    Instrument,
    /// The nth audio effect after the instrument.
    Effect(usize),
}

/// A request from the control thread to the audio callback. Everything shares one
/// queue so it's applied in the order it was sent — e.g. a param change queued for
/// a plugin can never land on whatever replaced it. Indices are valid for the
/// chain as it will be when the command is applied; the control side mirrors every
/// change to know that.
pub(crate) enum RackCommand {
    SetInstrument { slot: usize, plugin: Option<BoxedPlugin> },
    InsertEffect { slot: usize, section: Section, index: usize, plugin: BoxedPlugin },
    RemoveEffect { slot: usize, section: Section, index: usize },
    MoveEffect { slot: usize, section: Section, from: usize, to: usize },
    Param { slot: usize, position: ChainPosition, change: ParamChange },
    Filter { slot: usize, filter: MidiFilter },
    Gain { slot: usize, gain: f32 },
    /// Takes the plugin at `position` out of the chain, leaving its place empty
    /// (the chain skips it), and hands it to the control thread for work that
    /// mustn't overlap its processing (LV2 state save/restore).
    Lend { slot: usize, position: ChainPosition },
    /// Puts a lent plugin back in its place.
    Return { slot: usize, position: ChainPosition, plugin: BoxedPlugin },
}

/// One slot as the audio callback sees it.
struct SlotRuntime {
    /// Contiguous from `[0]`, so a removal shifts the ones after it down.
    midi_chain: [Option<BoxedPlugin>; MAX_MIDI_EFFECTS],
    /// `[0]` is the instrument; audio effects follow contiguously from `[1]`.
    chain: [Option<BoxedPlugin>; CHAIN_LEN],
    router: MidiRouter,
    /// This callback's MIDI after `router`, rebuilt every callback.
    midi: Vec<MidiEvent>,
    /// The MIDI chain's output for the current block.
    midi_scratch: Vec<MidiEvent>,
    /// Set when the MIDI chain changed: the instrument gets `ALL_NOTES_OFF` with
    /// the next block.
    reset_notes: bool,
    /// Gain applied at the end of the last rendered block; each block ramps from
    /// here to `target_gain` so fader moves don't click.
    gain: f32,
    target_gain: f32,
}

impl SlotRuntime {
    fn section_mut(&mut self, section: Section) -> &mut [Option<BoxedPlugin>] {
        match section {
            Section::Midi => &mut self.midi_chain,
            Section::Audio => &mut self.chain[1..],
        }
    }

    fn place_mut(&mut self, position: ChainPosition) -> Option<&mut Option<BoxedPlugin>> {
        match position {
            ChainPosition::MidiEffect(index) => self.midi_chain.get_mut(index),
            ChainPosition::Instrument => Some(&mut self.chain[0]),
            ChainPosition::Effect(index) => self.chain.get_mut(1 + index),
        }
    }

    fn plugin_mut(&mut self, position: ChainPosition) -> Option<&mut BoxedPlugin> {
        self.place_mut(position)?.as_mut()
    }
}

/// Runs `input` through every MIDI effect in `chain` in order, and returns what
/// comes out of the last one (`input` itself if there are none), preceded by
/// `ALL_NOTES_OFF` if `reset`. MIDI effects run every block, not only ones with
/// input, since some (arpeggiators, delays) produce MIDI on their own.
fn process_midi_chain<'a>(
    chain: &mut [Option<BoxedPlugin>; MAX_MIDI_EFFECTS],
    input: &'a [MidiEvent],
    scratch: &'a mut Vec<MidiEvent>,
    reset: bool,
    frames: usize,
) -> &'a [MidiEvent] {
    let mut in_scratch = false;
    for stage in chain.iter_mut().flatten() {
        stage.process(if in_scratch { scratch.as_slice() } else { input }, frames);
        scratch.clear();
        let room = scratch.capacity();
        scratch.extend(stage.midi_output().iter().take(room));
        in_scratch = true;
    }
    if !reset {
        let scratch: &'a Vec<MidiEvent> = scratch;
        return if in_scratch { scratch } else { input };
    }
    if !in_scratch {
        scratch.clear();
        let room = scratch.capacity() - ALL_NOTES_OFF.len();
        scratch.extend(input.iter().take(room));
    } else {
        scratch.truncate(scratch.capacity() - ALL_NOTES_OFF.len());
    }
    let events = scratch.len();
    scratch.extend(ALL_NOTES_OFF.iter().filter_map(|bytes| MidiEvent::from_raw(0, bytes)));
    let added = scratch.len() - events;
    scratch.rotate_right(added);
    scratch.as_slice()
}

/// Runs every plugin in `chain` in order, each fed the previous one's output (the
/// first one silence), and returns the index of the last one — whose output is the
/// slot's — or `None` if the chain is empty.
fn process_chain(
    chain: &mut [Option<BoxedPlugin>; CHAIN_LEN],
    midi: &[MidiEvent],
    frames: usize,
) -> Option<usize> {
    let mut previous: Option<usize> = None;
    for index in 0..CHAIN_LEN {
        let (before, rest) = chain.split_at_mut(index);
        let Some(stage) = rest[0].as_deref_mut() else {
            continue;
        };
        let source = previous.and_then(|previous| before[previous].as_deref());
        feed_inputs(stage, source, frames);
        // Effects get the slot's MIDI too: most ignore it, but some (vocoders,
        // MIDI-triggered gates) need it.
        stage.process(midi, frames);
        previous = Some(index);
    }
    previous
}

/// Copies `source`'s output into `stage`'s inputs, wrapping channels around when
/// the counts differ (mono → stereo duplicates, stereo → mono takes the left), or
/// zeroes them without a source.
fn feed_inputs(stage: &mut dyn HostedPlugin, source: Option<&dyn HostedPlugin>, frames: usize) {
    for channel in 0..stage.audio_input_count() {
        let input = &mut stage.audio_input_mut(channel)[..frames];
        match source {
            Some(source) if source.audio_output_count() > 0 => {
                let output = source.audio_output(channel % source.audio_output_count());
                input.copy_from_slice(&output[..frames]);
            }
            _ => input.fill(0.0),
        }
    }
}

/// Every slot's chain and mixer state, owned by the audio callback.
pub(crate) struct Rack {
    slots: [SlotRuntime; SLOT_COUNT],
    midi_batch: Vec<MidiEvent>,
    /// Plugins taken out of a chain, handed back to the control thread to drop —
    /// a plugin's teardown can deallocate or block.
    retired_tx: Producer<BoxedPlugin>,
    /// Plugins lent to the control thread (see [`RackCommand::Lend`]).
    lent_tx: Producer<BoxedPlugin>,
    channels: usize,
    max_block_size: usize,
    peaks: SlotPeaks,
}

impl Rack {
    pub fn new(
        settings: &[SlotSettings; SLOT_COUNT],
        retired_tx: Producer<BoxedPlugin>,
        lent_tx: Producer<BoxedPlugin>,
        channels: usize,
        max_block_size: usize,
        midi_capacity: usize,
        peaks: SlotPeaks,
    ) -> Self {
        Self {
            slots: std::array::from_fn(|slot| SlotRuntime {
                midi_chain: Default::default(),
                chain: Default::default(),
                router: MidiRouter::new(settings[slot].filter),
                midi: Vec::with_capacity(midi_capacity),
                // Room for a full callback's input plus `ALL_NOTES_OFF`.
                midi_scratch: Vec::with_capacity(midi_capacity + ALL_NOTES_OFF.len()),
                reset_notes: false,
                gain: settings[slot].gain,
                target_gain: settings[slot].gain,
            }),
            midi_batch: Vec::with_capacity(midi_capacity),
            retired_tx,
            lent_tx,
            channels,
            max_block_size,
            peaks,
        }
    }

    fn retire(&mut self, plugin: Option<BoxedPlugin>) {
        // The queue being full means the control thread hasn't collected in a long
        // while; leaking the plugin then beats running its teardown here.
        if let Some(plugin) = plugin
            && let Err(PushError::Full(plugin)) = self.retired_tx.push(plugin)
        {
            log::warn!("retired plugin queue full, leaking a plugin");
            std::mem::forget(plugin);
        }
    }

    /// Applies one command. Out-of-range indices are ignored rather than trusted —
    /// a panic here would take the audio thread down.
    pub fn apply(&mut self, command: RackCommand) {
        match command {
            RackCommand::SetInstrument { slot, plugin } => {
                let Some(runtime) = self.slots.get_mut(slot) else {
                    return;
                };
                // The new instrument never got the old one's note-ons.
                runtime.router.clear_held();
                let old = std::mem::replace(&mut runtime.chain[0], plugin);
                self.retire(old);
            }
            RackCommand::InsertEffect {
                slot,
                section,
                index,
                plugin,
            } => {
                let Some(runtime) = self.slots.get_mut(slot) else {
                    return self.retire(Some(plugin));
                };
                let chain = runtime.section_mut(section);
                if index < chain.len() && chain[chain.len() - 1].is_none() {
                    chain[index..].rotate_right(1);
                    chain[index] = Some(plugin);
                    runtime.reset_notes |= section == Section::Midi;
                } else {
                    self.retire(Some(plugin));
                }
            }
            RackCommand::RemoveEffect {
                slot,
                section,
                index,
            } => {
                let Some(runtime) = self.slots.get_mut(slot) else {
                    return;
                };
                let chain = runtime.section_mut(section);
                if index >= chain.len() {
                    return;
                }
                let old = chain[index].take();
                chain[index..].rotate_left(1);
                runtime.reset_notes |= section == Section::Midi;
                self.retire(old);
            }
            RackCommand::MoveEffect {
                slot,
                section,
                from,
                to,
            } => {
                let Some(runtime) = self.slots.get_mut(slot) else {
                    return;
                };
                let chain = runtime.section_mut(section);
                if from < chain.len() && to < chain.len() {
                    if from < to {
                        chain[from..=to].rotate_left(1);
                    } else {
                        chain[to..=from].rotate_right(1);
                    }
                    runtime.reset_notes |= section == Section::Midi;
                }
            }
            RackCommand::Param {
                slot,
                position,
                change,
            } => {
                if let Some(plugin) = self
                    .slots
                    .get_mut(slot)
                    .and_then(|runtime| runtime.plugin_mut(position))
                {
                    plugin.set_param(change.param_id, change.value);
                }
            }
            RackCommand::Filter { slot, filter } => {
                if let Some(runtime) = self.slots.get_mut(slot) {
                    runtime.router.set_filter(filter);
                }
            }
            RackCommand::Gain { slot, gain } => {
                if let Some(runtime) = self.slots.get_mut(slot) {
                    runtime.target_gain = gain;
                }
            }
            RackCommand::Lend { slot, position } => {
                let Some(place) = self.slots.get_mut(slot).and_then(|runtime| runtime.place_mut(position))
                else {
                    return;
                };
                if let Some(plugin) = place.take()
                    && let Err(PushError::Full(plugin)) = self.lent_tx.push(plugin)
                {
                    // Nobody's waiting for this many; keep playing it.
                    *place = Some(plugin);
                }
            }
            RackCommand::Return {
                slot,
                position,
                plugin,
            } => {
                let Some(runtime) = self.slots.get_mut(slot) else {
                    return self.retire(Some(plugin));
                };
                // Notes that ended while it was away never reached it.
                runtime.reset_notes |= position == ChainPosition::Instrument;
                match runtime.place_mut(position) {
                    Some(place) if place.is_none() => *place = Some(plugin),
                    _ => self.retire(Some(plugin)),
                }
            }
        }
    }

    /// Renders one callback: drains MIDI, runs every slot's chain, and sums the
    /// slots' outputs (each at its gain) into the interleaved `data`.
    pub fn render(&mut self, midi_rx: &mut MidiConsumer, data: &mut [f32]) {
        self.midi_batch.clear();
        while let Ok(event) = midi_rx.pop() {
            self.midi_batch.push(event);
        }
        for runtime in &mut self.slots {
            runtime.midi.clear();
            for event in &self.midi_batch {
                if runtime.router.accept(event) {
                    runtime.midi.push(*event);
                }
            }
        }

        let channels = self.channels;
        // The backend can hand back a buffer spanning more frames than
        // `max_block_size` in one callback (observed with a Bluetooth output
        // device, whose negotiated period can reach/exceed it) — render it as
        // multiple `process` calls instead of silently leaving the tail of `data`
        // unwritten. Only the first sub-block gets this callback's freshly drained
        // MIDI; later sub-blocks get none, since no further real time has passed
        // for them (see "MIDI events always get frame_offset = 0" in CLAUDE.md).
        for (chunk_index, data_chunk) in data.chunks_mut(channels * self.max_block_size).enumerate()
        {
            let frames = data_chunk.len() / channels;
            data_chunk.fill(0.0);
            for (slot, runtime) in self.slots.iter_mut().enumerate() {
                let input: &[MidiEvent] = if chunk_index == 0 { &runtime.midi } else { &[] };
                let midi = process_midi_chain(
                    &mut runtime.midi_chain,
                    input,
                    &mut runtime.midi_scratch,
                    std::mem::take(&mut runtime.reset_notes),
                    frames,
                );
                let Some(last) = process_chain(&mut runtime.chain, midi, frames) else {
                    continue;
                };
                let output_stage = runtime.chain[last].as_deref().unwrap();
                let output_channels = output_stage.audio_output_count();
                if output_channels > 0 {
                    let gain_step = (runtime.target_gain - runtime.gain) / frames as f32;
                    let mut peak = 0.0f32;
                    for channel in 0..channels {
                        let rendered = output_stage.audio_output(channel % output_channels);
                        for (frame_index, (frame, sample)) in
                            data_chunk.chunks_mut(channels).zip(rendered).enumerate()
                        {
                            let gain = runtime.gain + gain_step * frame_index as f32;
                            frame[channel] += sample * gain;
                            peak = peak.max((sample * gain).abs());
                        }
                    }
                    // `max` drops NaN; a NaN-producing plugin reads as silent.
                    self.peaks[slot].fetch_max(peak.to_bits(), Ordering::Relaxed);
                }
                runtime.gain = runtime.target_gain;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use live_keys_core::{midi_ring_buffer, ring_buffer};

    const BLOCK: usize = 16;

    /// Outputs `op` applied to its input (a source ignores input). Stereo in/out.
    struct Fake {
        op: fn(f32) -> f32,
        inputs: Vec<Vec<f32>>,
        outputs: Vec<Vec<f32>>,
        frames: usize,
    }

    fn fake(op: fn(f32) -> f32) -> BoxedPlugin {
        Box::new(Fake {
            op,
            inputs: vec![vec![0.0; BLOCK]; 2],
            outputs: vec![vec![0.0; BLOCK]; 2],
            frames: 0,
        })
    }

    impl HostedPlugin for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn audio_output_count(&self) -> usize {
            2
        }
        fn audio_input_count(&self) -> usize {
            2
        }
        fn audio_input_mut(&mut self, channel: usize) -> &mut [f32] {
            &mut self.inputs[channel]
        }
        fn process(&mut self, _midi_in: &[MidiEvent], frames: usize) {
            for (input, output) in self.inputs.iter().zip(&mut self.outputs) {
                for (i, o) in input[..frames].iter().zip(&mut output[..frames]) {
                    *o = (self.op)(*i);
                }
            }
            self.frames = frames;
        }
        fn audio_output(&self, channel: usize) -> &[f32] {
            &self.outputs[channel][..self.frames]
        }
        fn set_param(&mut self, _id: u32, _value: f32) {}
    }

    fn rack() -> Rack {
        let (retired_tx, _retired_rx) = ring_buffer(64);
        let (lent_tx, _lent_rx) = ring_buffer(4);
        Rack::new(&Default::default(), retired_tx, lent_tx, 2, BLOCK, 16, Default::default())
    }

    /// Renders one block and returns slot 0's first output sample (every slot but
    /// 0 is empty).
    fn render(rack: &mut Rack) -> f32 {
        let (_midi_tx, mut midi_rx) = midi_ring_buffer(1);
        let mut data = vec![0.0; 2 * BLOCK];
        rack.render(&mut midi_rx, &mut data);
        assert!(data.iter().all(|&s| s == data[0]), "every sample should match");
        data[0]
    }

    fn insert(rack: &mut Rack, index: usize, op: fn(f32) -> f32) {
        rack.apply(RackCommand::InsertEffect {
            slot: 0,
            section: Section::Audio,
            index,
            plugin: fake(op),
        });
    }

    const ONE: fn(f32) -> f32 = |_| 1.0;
    const PLUS_ONE: fn(f32) -> f32 = |x| x + 1.0;
    const TIMES_TWO: fn(f32) -> f32 = |x| x * 2.0;

    #[test]
    fn effects_process_in_chain_order() {
        let mut rack = rack();
        rack.apply(RackCommand::SetInstrument {
            slot: 0,
            plugin: Some(fake(ONE)),
        });
        assert_eq!(render(&mut rack), 1.0);
        insert(&mut rack, 0, PLUS_ONE);
        insert(&mut rack, 1, TIMES_TWO);
        assert_eq!(render(&mut rack), 4.0); // (1 + 1) * 2

        rack.apply(RackCommand::MoveEffect {
            slot: 0,
            section: Section::Audio,
            from: 1,
            to: 0,
        });
        assert_eq!(render(&mut rack), 3.0); // 1 * 2 + 1

        rack.apply(RackCommand::RemoveEffect {
            slot: 0,
            section: Section::Audio,
            index: 0,
        });
        assert_eq!(render(&mut rack), 2.0); // 1 + 1

        insert(&mut rack, 0, TIMES_TWO);
        assert_eq!(render(&mut rack), 3.0); // 1 * 2 + 1
    }

    #[test]
    fn chain_without_instrument_starts_from_silence() {
        let mut rack = rack();
        insert(&mut rack, 0, PLUS_ONE);
        assert_eq!(render(&mut rack), 1.0);
        rack.apply(RackCommand::SetInstrument {
            slot: 0,
            plugin: Some(fake(ONE)),
        });
        assert_eq!(render(&mut rack), 2.0);
    }

    #[test]
    fn full_chain_rejects_another_effect() {
        let mut rack = rack();
        for index in 0..MAX_EFFECTS {
            insert(&mut rack, index, PLUS_ONE);
        }
        insert(&mut rack, 0, TIMES_TWO);
        assert_eq!(render(&mut rack), MAX_EFFECTS as f32);
    }

    /// A MIDI effect applying `op` to every note number.
    struct FakeMidiEffect {
        op: fn(u8) -> u8,
        out: Vec<MidiEvent>,
    }

    impl HostedPlugin for FakeMidiEffect {
        fn name(&self) -> &str {
            "fake midi"
        }
        fn audio_output_count(&self) -> usize {
            0
        }
        fn audio_input_count(&self) -> usize {
            0
        }
        fn audio_input_mut(&mut self, _channel: usize) -> &mut [f32] {
            &mut []
        }
        fn process(&mut self, midi_in: &[MidiEvent], _frames: usize) {
            self.out.clear();
            for event in midi_in {
                let mut bytes = [0; 3];
                bytes[..event.as_slice().len()].copy_from_slice(event.as_slice());
                if bytes[0] & 0xE0 == 0x80 {
                    bytes[1] = (self.op)(bytes[1]);
                }
                self.out.extend(MidiEvent::from_raw(0, &bytes[..event.as_slice().len()]));
            }
        }
        fn audio_output(&self, _channel: usize) -> &[f32] {
            &[]
        }
        fn midi_output(&self) -> &[MidiEvent] {
            &self.out
        }
        fn set_param(&mut self, _id: u32, _value: f32) {}
    }

    /// An instrument whose output is the last note-on it got, or -1 right after
    /// an all-notes-off on channel 1.
    struct FakeInstrument {
        level: f32,
        outputs: Vec<f32>,
    }

    impl HostedPlugin for FakeInstrument {
        fn name(&self) -> &str {
            "fake instrument"
        }
        fn audio_output_count(&self) -> usize {
            1
        }
        fn audio_input_count(&self) -> usize {
            0
        }
        fn audio_input_mut(&mut self, _channel: usize) -> &mut [f32] {
            &mut []
        }
        fn process(&mut self, midi_in: &[MidiEvent], frames: usize) {
            for event in midi_in {
                match event.as_slice() {
                    [0x90, note, _] => self.level = f32::from(*note),
                    [0xB0, 123, 0] => self.level = -1.0,
                    _ => {}
                }
            }
            self.outputs[..frames].fill(self.level);
        }
        fn audio_output(&self, _channel: usize) -> &[f32] {
            &self.outputs
        }
        fn set_param(&mut self, _id: u32, _value: f32) {}
    }

    #[test]
    fn midi_effects_process_in_order_and_reset_notes_on_change() {
        let mut rack = rack();
        rack.apply(RackCommand::SetInstrument {
            slot: 0,
            plugin: Some(Box::new(FakeInstrument {
                level: 0.0,
                outputs: vec![0.0; BLOCK],
            })),
        });
        for (index, op) in [(0, (|n| n + 1) as fn(u8) -> u8), (1, |n| n * 2)] {
            rack.apply(RackCommand::InsertEffect {
                slot: 0,
                section: Section::Midi,
                index,
                plugin: Box::new(FakeMidiEffect {
                    op,
                    out: Vec::with_capacity(16),
                }),
            });
        }
        let (mut midi_tx, mut midi_rx) = midi_ring_buffer(4);
        let mut play = |rack: &mut Rack, note: Option<u8>| {
            if let Some(note) = note {
                midi_tx.push(MidiEvent::from_raw(0, &[0x90, note, 100]).unwrap()).unwrap();
            }
            let mut data = vec![0.0; 2 * BLOCK];
            rack.render(&mut midi_rx, &mut data);
            data[0]
        };

        // The inserts changed the MIDI chain: the instrument is reset first, then
        // gets the note.
        assert_eq!(play(&mut rack, Some(10)), 22.0); // (10 + 1) * 2
        rack.apply(RackCommand::MoveEffect {
            slot: 0,
            section: Section::Midi,
            from: 1,
            to: 0,
        });
        assert_eq!(play(&mut rack, None), -1.0); // reset, no note
        assert_eq!(play(&mut rack, Some(10)), 21.0); // 10 * 2 + 1
    }
}
