//! Manual smoke test: scans for VST3 plugins, then instantiates every instrument
//! found, plays a note into it offline, and reports the output's peak level.
//! `cargo run -p live-keys-vst3-host --example smoke --release`

use live_keys_core::{HostedPlugin, MidiEvent};
use live_keys_vst3_host::Vst3Host;

const BLOCK: usize = 512;
const SAMPLE_RATE: f64 = 48_000.0;

fn main() {
    env_logger::init();
    let host = Vst3Host::new(BLOCK, &[]);
    for info in host.list_plugins() {
        println!(
            "{} [{}] {}",
            info.id,
            if info.is_instrument { "instrument" } else { "effect" },
            info.name
        );
    }

    for info in host.list_plugins().into_iter().filter(|info| info.is_instrument) {
        let (mut plugin, controller) = match host.instantiate(&info.id, SAMPLE_RATE) {
            Ok(pair) => pair,
            Err(err) => {
                println!("{}: FAILED to instantiate: {err:#}", info.name);
                continue;
            }
        };
        // Sample-based instruments may still be loading in the background.
        // Processing (silence) at roughly real-time pace meanwhile, since some only
        // finish loading once they see the audio thread running.
        if let Ok(secs) = std::env::var("SMOKE_WAIT_SECS") {
            let block_time = std::time::Duration::from_secs_f64(BLOCK as f64 / SAMPLE_RATE);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs.parse().unwrap());
            while std::time::Instant::now() < deadline {
                plugin.process(&[], BLOCK);
                std::thread::sleep(block_time);
            }
        }
        let note_on = [
            MidiEvent::from_raw(0, &[0x90, 48, 127]).unwrap(),
            MidiEvent::from_raw(0, &[0x90, 60, 127]).unwrap(),
            MidiEvent::from_raw(0, &[0x90, 72, 127]).unwrap(),
        ];
        let note_off = [
            MidiEvent::from_raw(0, &[0x80, 48, 0]).unwrap(),
            MidiEvent::from_raw(0, &[0x80, 60, 0]).unwrap(),
            MidiEvent::from_raw(0, &[0x80, 72, 0]).unwrap(),
        ];
        let mut peak = 0.0f32;
        // ~2 s of chord, then release.
        for block in 0..220 {
            let midi: &[MidiEvent] = match block {
                0 => &note_on,
                190 => &note_off,
                _ => &[],
            };
            plugin.process(midi, BLOCK);
            for ch in 0..plugin.audio_output_count() {
                peak = plugin.audio_output(ch).iter().fold(peak, |p, s| p.max(s.abs()));
            }
        }
        println!(
            "{}: {} output channels, {} params, peak {peak:.4}",
            info.name,
            plugin.audio_output_count(),
            controller.params().len()
        );
    }
}
