//! Manual smoke test for GUIs that talk to their DSP through atom ports only:
//! loads setBfree into a live engine, opens its GUI and passes atoms both ways
//! (GUI → `control` input, `notify` output → GUI) for N seconds (default 10),
//! printing what crossed. `cargo run -p live-keys-lv2-ui --example test_setbfree
//! --release -- [seconds] [plugin uri]` (needs an audio device and a MIDI port,
//! like the app). Another plugin's URI runs the same through its GUI.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use live_keys_engine::{Engine, PluginEditor};
use live_keys_lv2_ui::{LaunchUiRequest, PortMessage};

const SETBFREE_URI: &str = "http://gareus.org/oss/lv2/b_synth";

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut engine = Engine::new(8192);
    engine.start(None, None)?;
    let uri = std::env::args().nth(2).unwrap_or_else(|| SETBFREE_URI.to_string());
    let key = engine.load_instrument(0, &uri)?;

    let (tx, rx) = mpsc::channel::<PortMessage>();
    let handle = {
        let meta = engine.plugin(key).unwrap();
        let PluginEditor::Lv2 { plugin, features, raw_handle, .. } = &meta.editor else {
            anyhow::bail!("not LV2");
        };
        live_keys_lv2_ui::launch_ui(LaunchUiRequest {
            plugin,
            features: features.clone(),
            instance_handle: *raw_handle,
            port_write: Box::new(move |message| {
                let _ = tx.send(message);
            }),
        })?
    };
    println!("GUI open for {seconds}s");

    let (mut to_dsp, mut to_gui, mut largest) = (0, 0, 0);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) && !handle.is_closed() {
        while let Ok(message) = rx.try_recv() {
            match message {
                PortMessage::Atom { port, atom } => {
                    to_dsp += 1;
                    engine.send_ui_atom(key, port, &atom);
                }
                PortMessage::Control { port, value } => {
                    to_dsp += 1;
                    engine.send_param_change(key, port, value);
                }
            }
        }
        engine.drain_ui_events(|_, port, event| {
            to_gui += 1;
            largest = largest.max(event.size);
            handle.notify(PortMessage::Atom { port, atom: event.to_atom() });
        });
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("atoms GUI → DSP: {to_dsp}, DSP → GUI: {to_gui} (largest body {largest} bytes)");
    // The GUI tears down on its own thread and may still read the plugin
    // instance (Surge XT does), so give it time before the engine frees it.
    drop(handle);
    std::thread::sleep(Duration::from_secs(1));
    Ok(())
}
