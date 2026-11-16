//! DSP building blocks for live-keys' own instruments: filters, LFOs and
//! envelopes, plus unit conversions. Shared by the SFZ engine and any synth
//! written later, so nothing here knows about SFZ or a plugin format.
//!
//! Everything is real-time safe (no allocation, no locks) and `Copy`, so a
//! voice can hold its own instances inline. Per-sample methods (`process`,
//! `tick`) sit next to cheaper ways to step at control rate (`advance`), and
//! parameters can be changed while running.

pub mod convert;
pub mod envelope;
pub mod filter;
pub mod lfo;

pub use envelope::{Dahdsr, Envelope};
pub use filter::{Filter, FilterMode};
pub use lfo::{Lfo, Waveform};
