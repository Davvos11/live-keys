//! Manual smoke test for presets through the running engine: loads each plugin
//! given (default: an LV2 one without state, an LV2 one with state, a VST3 one)
//! into slot 0 of a live stream, applies its first preset, saves a user preset,
//! reloads it and deletes it again.
//! `cargo run -p live-keys-engine --example presets --release -- [plugin id]...`

use live_keys_engine::Engine;

fn main() -> anyhow::Result<()> {
    let mut ids: Vec<String> = std::env::args().skip(1).collect();
    if ids.is_empty() {
        ids = [
            "http://drobilla.net/plugins/mda/EPiano",
            "https://surge-synthesizer.github.io/lv2/surge-xt",
        ]
        .map(String::from)
        .to_vec();
        if let Some(vst3) = Engine::new(8192).list_plugins().into_iter().find(|p| p.name == "Surge XT" && p.id.starts_with("vst3:")) {
            ids.push(vst3.id);
        }
    }
    let mut engine = Engine::new(8192);
    engine.start(None, None)?;
    for id in ids {
        let key = engine.load_instrument(0, &id)?;
        let meta = engine.plugin(key).unwrap();
        let (name, first_param) = (meta.name.clone(), meta.params.first().map(|p| p.id));
        let presets = engine.presets(key);
        println!("{name}: {} presets", presets.len());
        for preset in presets.iter().take(5) {
            println!("  {} (user: {})", preset.name, preset.user);
        }
        if let Some(preset) = presets.first() {
            let started = std::time::Instant::now();
            engine.load_preset(key, &preset.id)?;
            println!("  loaded \"{}\" in {:?}", preset.name, started.elapsed());
        }
        if let Some(param) = first_param {
            let before = engine.param_value(key, param);
            engine.set_param(key, param, 0.25);
            let saved = engine.save_preset(key, "Smoke Test")?;
            println!("  saved {} (first param was {before:?}, set to 0.25)", saved.id);
            engine.set_param(key, param, 0.75);
            engine.load_preset(key, &saved.id)?;
            println!("  reloaded: first param now {:?}", engine.param_value(key, param));
            println!("  listed: {}", engine.presets(key).iter().any(|p| p.id == saved.id));
            engine.delete_preset(key, &saved.id)?;
            println!("  deleted, still listed: {}", engine.presets(key).iter().any(|p| p.id == saved.id));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Ok(())
}
