//! Loads an SFZ instrument, plays a short passage offline and writes it to a WAV
//! file, reporting load time, memory and how fast it renders.
//!
//! cargo run -p live-keys-sfz --example render --release -- <file.sfz> [out.wav]

use anyhow::{Context, Result};
use live_keys_sfz::{Instrument, Synth};
use std::sync::Arc;
use std::time::Instant;

const RATE: f32 = 48000.0;
const BLOCK: usize = 256;

/// (time in seconds, MIDI bytes)
fn passage() -> Vec<(f32, [u8; 3])> {
    let mut events = Vec::new();
    let mut note = |t: f32, len: f32, key: u8, vel: u8| {
        events.push((t, [0x90, key, vel]));
        events.push((t + len, [0x80, key, 0]));
    };
    // Velocity sweep on middle C.
    for (i, vel) in [20, 45, 70, 95, 120].into_iter().enumerate() {
        note(i as f32 * 0.6, 0.5, 60, vel);
    }
    // A chord, held by the pedal after the keys go up.
    for key in [48, 55, 60, 64, 67] {
        note(3.5, 0.3, key, 80);
    }
    // A fast run across the keyboard.
    for i in 0..24 {
        note(6.0 + i as f32 * 0.08, 0.07, 36 + i * 3, 90);
    }
    events.push((3.4, [0xb0, 64, 127]));
    events.push((5.5, [0xb0, 64, 0]));
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    events
}

fn main() -> Result<()> {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let path = args.next().context("usage: render <file.sfz> [out.wav]")?;
    let out = args.next().unwrap_or_else(|| "render.wav".into());

    let start = Instant::now();
    let instrument = Arc::new(Instrument::load(path.as_ref())?);
    println!(
        "loaded {} regions, {} samples ({:.0} MB) in {:.2} s",
        instrument.region_count(),
        instrument.sample_count(),
        instrument.sample_memory() as f64 / 1e6,
        start.elapsed().as_secs_f64()
    );

    let mut synth = Synth::new(instrument, RATE, 256);
    let events = passage();
    let seconds = events.last().map_or(0.0, |e| e.0) + 4.0;
    let total = (seconds * RATE) as usize;
    let (mut left, mut right) = (vec![0.0f32; total], vec![0.0f32; total]);
    let mut next = 0;
    let mut most_voices = 0;
    let start = Instant::now();
    for block in (0..total).step_by(BLOCK) {
        while next < events.len() && (events[next].0 * RATE) as usize <= block {
            synth.midi(&events[next].1);
            next += 1;
        }
        let end = (block + BLOCK).min(total);
        synth.render(&mut left[block..end], &mut right[block..end]);
        most_voices = most_voices.max(synth.active_voices());
    }
    let took = start.elapsed().as_secs_f64();
    println!(
        "rendered {seconds:.1} s in {took:.3} s ({:.0}× real time), at most {most_voices} voices",
        seconds as f64 / took
    );

    let peak = left.iter().chain(&right).fold(0.0f32, |m, s| m.max(s.abs()));
    println!("peak {:.1} dBFS", 20.0 * peak.log10());
    let spec = hound::WavSpec { channels: 2, sample_rate: RATE as u32, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut writer = hound::WavWriter::create(&out, spec)?;
    for (l, r) in left.iter().zip(&right) {
        writer.write_sample(*l)?;
        writer.write_sample(*r)?;
    }
    writer.finalize()?;
    println!("wrote {out}");
    Ok(())
}
