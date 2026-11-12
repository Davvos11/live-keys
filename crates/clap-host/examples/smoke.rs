//! Scans for CLAP plugins, then renders a chord offline through every
//! instrument found (or those whose name contains the first argument), printing
//! the peak level, its parameter count, a state round trip and its presets.

use live_keys_clap_host::ClapHost;
use live_keys_core::{HostedPlugin, MidiEvent, PluginKind};

const BLOCK: usize = 512;

fn main() {
    env_logger::init();
    let filter = std::env::args().nth(1).unwrap_or_default().to_lowercase();
    let host = ClapHost::new(BLOCK, &[]);
    for info in host.list_plugins() {
        println!("{}\t{:?}\t{}", info.id, info.kind, info.name);
    }
    for info in host.list_plugins() {
        if info.kind != PluginKind::Instrument || !info.name.to_lowercase().contains(&filter) {
            continue;
        }
        let (mut plugin, controller) = match host.instantiate(&info.id, 48_000.0) {
            Ok(pair) => pair,
            Err(err) => {
                println!("{}: failed: {err:#}", info.name);
                continue;
            }
        };
        let chord: Vec<MidiEvent> =
            [60, 64, 67].iter().filter_map(|&note| MidiEvent::from_raw(0, &[0x90, note, 100])).collect();
        let mut peak = 0.0f32;
        for block in 0..200 {
            plugin.process(if block == 10 { &chord } else { &[] }, BLOCK);
            for channel in 0..plugin.audio_output_count() {
                peak = plugin.audio_output(channel).iter().fold(peak, |peak, s| peak.max(s.abs()));
            }
            let _ = controller.idle(|_, _| {});
        }
        let state = controller.state().map(|s| s.len());
        let restored = controller.state().and_then(|s| controller.set_state(&s));
        let started = std::time::Instant::now();
        let presets = host.presets(&info.id);
        println!(
            "{}: {} outputs, peak {peak:.3}, {} params, state {state:?} restore {restored:?}, {} presets in {:?} (e.g. {:?})",
            info.name,
            plugin.audio_output_count(),
            controller.params().len(),
            presets.len(),
            started.elapsed(),
            presets.iter().take(3).map(|p| &p.name).collect::<Vec<_>>()
        );
        if let Some(preset) = presets.first() {
            println!("  load {:?}: {:?}", preset.name, host.load_preset(&controller, &info.id, &preset.id));
        }
        drop(plugin);
        drop(controller);
        live_keys_clap_host::destroy_pending();
    }
}
