//! Manual smoke test for the engine's side of setBfree's control messages
//! (headless, no GUI): loads setBfree into slot 0 of a live stream and prints
//! what its `uiinit` dump reported, sets a drawbar and reads it back, then saves
//! a state file, changes the drawbar again and restores the file (setBfree
//! reports `special.reinit`, then everything). `RUST_LOG=live_keys_engine=trace`
//! prints every message. `cargo run -p live-keys-engine --example setbfree
//! --release` (needs an audio device).

use std::time::Duration;

use live_keys_engine::{Engine, PluginKey, SETBFREE_URI};

const DRAWBAR: &str = "upper.drawbar16";

/// Lets the plugin run a while, then records what it reported: the number of
/// events and whether any recorded value changed.
fn settle(engine: &mut Engine) -> (usize, bool) {
    std::thread::sleep(Duration::from_millis(300));
    let mut events = 0;
    engine.drain_ui_events(|_, _, _| events += 1);
    (events, !engine.take_setbfree_changes().is_empty())
}

fn show(engine: &Engine, key: PluginKey, what: &str) {
    let functions =
        [DRAWBAR, "upper.drawbar8", "percussion.enable", "vibrato.knob", "rotary.speed-preset", "reverb.mix"];
    let values: Vec<String> = functions.iter().map(|f| format!("{f}={:?}", engine.setbfree_value(key, f))).collect();
    println!("{what}: {}", values.join(" "));
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let mut engine = Engine::new(8192);
    engine.start(None)?;
    let key = engine.load_instrument(0, SETBFREE_URI)?;
    let (events, changed) = settle(&mut engine);
    println!("after loading: {events} events, values changed: {changed}");
    show(&engine, key, "  values");

    engine.send_setbfree(key, DRAWBAR, 64);
    let (events, changed) = settle(&mut engine);
    println!("after setting {DRAWBAR} to 64: {events} events, values changed: {changed}");
    show(&engine, key, "  values");

    let dir = std::env::temp_dir().join(format!("live-keys-setbfree-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let file = engine.save_state_file(key, &dir, "state")?;
    settle(&mut engine);
    engine.send_setbfree(key, DRAWBAR, 127);
    settle(&mut engine);
    show(&engine, key, &format!("after saving, then setting {DRAWBAR} to 127"));
    engine.load_state_file(key, &file)?;
    let (events, changed) = settle(&mut engine);
    println!("after restoring the state file: {events} events, values changed: {changed}");
    show(&engine, key, "  values");
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
