//! An SFZ sampler engine: the sound core of live-keys' own sampled instruments
//! (the Salamander piano first; see `docs/design/own-plugins.md`).
//!
//! No host code: load an [`Instrument`] (off the audio thread), make a [`Synth`]
//! for it, feed it MIDI and call [`Synth::render`] on the audio thread. A plugin
//! front-end (built into live-keys, or a standalone CLAP/VST3/LV2 build) wraps it.
//!
//! What's supported is the SFZ subset that real instruments use, starting with
//! everything the Salamander Grand Piano's SFZ needs: `#define`/`#include`,
//! the `<control>`/`<global>`/`<master>`/`<group>`/`<region>`/`<curve>`
//! hierarchy, key/velocity/channel/controller/random/round-robin conditions,
//! keyswitches, release triggers with `rt_decay`, controller-triggered regions
//! (pedal noises), groups with `off_by` and (note) polyphony, velocity curves
//! and crossfades, controller modulation with curves, and the amplitude
//! envelope. Generators (`sample=*sine`…) are the hook for modelled layers.
//! Samples are streamed from disk: only each one's head stays in memory.
//! Not yet: filters, LFOs and other envelopes, effects.

mod curve;
mod envelope;
mod instrument;
pub mod parse;
mod region;
mod sample;
mod stream;
mod synth;

pub use curve::Curve;
pub use instrument::{CC_COUNT, DEFAULT_HEAD_FRAMES, Instrument, Preload};
pub use region::{Generator, LoopMode, Region, Trigger};
pub use sample::Sample;
pub use synth::Synth;
