//! Manual check that LV2 plugins follow the host's tempo: x42's step sequencer
//! (synced to the host, a quarter note per step) with every other step of a
//! row on, rendered offline for 4 s at 120 BPM, at 240 BPM, and stopped;
//! prints how many notes it played each time: 5 and 9 (every other beat, counting
//! both ends), and none when stopped.
//! `cargo run -p live-keys-lv2-host --example transport --release`

use live_keys_core::{HostedPlugin, Transport};
use live_keys_lv2_host::Lv2Host;

const URI: &str = "http://gareus.org/oss/lv2/stepseq#s8n8";
const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;

fn main() -> anyhow::Result<()> {
    let host = Lv2Host::new(BLOCK);
    for (tempo, playing) in [(120.0, true), (240.0, true), (120.0, false)] {
        let mut plugin = host.instantiate(URI, RATE)?;
        let id = |symbol: &str| plugin.params().iter().find(|p| p.symbol == symbol).map(|p| p.id);
        let sync = id("sync").expect("stepseq has a sync switch");
        // Every other step: consecutive steps of one pitch are one held note.
        let row: Vec<u32> = [1, 3, 5, 7].iter().filter_map(|step| id(&format!("grid_{step}_1"))).collect();
        plugin.set_param(sync, 1.0);
        for cell in row {
            plugin.set_param(cell, 100.0);
        }
        let mut transport = Transport { tempo, playing, ..Transport::default() };
        let blocks = (4.0 * RATE) as usize / BLOCK;
        let mut notes = 0;
        for _ in 0..blocks {
            plugin.set_transport(&transport);
            plugin.process(&[], BLOCK);
            notes += plugin.midi_output().iter().filter(|event| event.as_slice()[0] & 0xF0 == 0x90 && event.as_slice()[2] > 0).count();
            if playing {
                transport.frame += BLOCK as i64;
                transport.beats += BLOCK as f64 * tempo / 60.0 / RATE;
                let bar = (transport.beats / 4.0).floor();
                transport.bar = bar as i64;
                transport.bar_start = bar * 4.0;
            }
        }
        println!("{tempo} BPM, {}: {notes} notes in 4 s", if playing { "playing" } else { "stopped" });
    }
    Ok(())
}
