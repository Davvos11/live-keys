//! Plays an SFZ instrument live: every MIDI input (or the ones whose name
//! contains a filter) into the default audio output. Enter quits.
//!
//! cargo run -p live-keys-sfz --example play --release -- <file.sfz> [midi port filter]

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use live_keys_sfz::{Instrument, Synth};
use std::sync::Arc;

const BLOCK: usize = 512;

fn main() -> Result<()> {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let path = args.next().context("usage: play <file.sfz> [midi port filter]")?;
    let filter = args.next().map(|f| f.to_lowercase());

    println!("loading {path}…");
    let instrument = Arc::new(Instrument::load(path.as_ref())?);
    println!("{} regions, {:.0} MB of samples", instrument.region_count(), instrument.sample_memory() as f64 / 1e6);

    let (midi_tx, mut midi_rx) = rtrb::RingBuffer::<[u8; 3]>::new(1024);
    let midi_tx = Arc::new(std::sync::Mutex::new(midi_tx));
    let midi = midir::MidiInput::new("live-keys-sfz")?;
    let mut connections = Vec::new();
    for port in midi.ports() {
        let name = midi.port_name(&port)?;
        let wanted = match &filter {
            Some(f) => name.to_lowercase().contains(f),
            None => !name.contains("Midi Through"),
        };
        if !wanted {
            continue;
        }
        let tx = midi_tx.clone();
        let input = midir::MidiInput::new("live-keys-sfz")?;
        match input.connect(
            &port,
            "in",
            move |_, bytes, _| {
                // Channel messages only (no sysex or real-time bytes).
                if let Some(&status) = bytes.first()
                    && (0x80..0xf0).contains(&status)
                {
                    let byte = |i: usize| bytes.get(i).copied().unwrap_or(0);
                    let _ = tx.lock().unwrap().push([status, byte(1), byte(2)]);
                }
            },
            (),
        ) {
            Ok(connection) => {
                println!("listening to {name}");
                connections.push(connection);
            }
            Err(err) => eprintln!("{name}: {err}"),
        }
    }
    if connections.is_empty() {
        println!("no MIDI input connected");
    }

    let device = cpal::default_host().default_output_device().context("no audio output")?;
    let config = device.default_output_config()?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate() as f32;
    println!("output at {rate} Hz, {channels} channels");
    let mut synth = Synth::new(instrument, rate, 256);
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let stream = device.build_output_stream(
        config.into(),
        move |data: &mut [f32], _| {
            while let Ok(bytes) = midi_rx.pop() {
                synth.midi(&bytes);
            }
            for chunk in data.chunks_mut(BLOCK * channels) {
                let frames = chunk.len() / channels;
                synth.render(&mut left[..frames], &mut right[..frames]);
                for (i, frame) in chunk.chunks_mut(channels).enumerate() {
                    frame[0] = left[i];
                    if channels > 1 {
                        frame[1] = right[i];
                    }
                    frame.iter_mut().skip(2).for_each(|s| *s = 0.0);
                }
            }
        },
        |err| eprintln!("audio: {err}"),
        None,
    )?;
    stream.play()?;
    println!("playing; press Enter to quit");
    std::io::stdin().read_line(&mut String::new())?;
    Ok(())
}
