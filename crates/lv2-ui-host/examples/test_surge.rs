//! Manual smoke test for the X11UI hosting path: instantiates Surge XT and opens
//! its own GUI window for a few seconds. Run with `cargo run --example test_surge`.
//!
//! Surge XT has audio input ports (sidechain/audio-rate modulation), so it's
//! rejected by `live_keys_lv2_host::Lv2Host::instantiate`'s instruments-only
//! constraint — that restriction is unrelated to UI hosting, so this drives `livi`
//! directly instead, bypassing it for this test only.

use live_keys_lv2_ui::LaunchUiRequest;

const SURGE_URI: &str = "https://surge-synthesizer.github.io/lv2/surge-xt";
const MAX_BLOCK_SIZE: usize = 8192;
const SAMPLE_RATE: f64 = 48000.0;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let world = livi::World::new();
    let plugin = world
        .plugin_by_uri(SURGE_URI)
        .ok_or_else(|| anyhow::anyhow!("plugin not found: {SURGE_URI}"))?;
    let features = world.build_features(livi::FeaturesBuilder {
        min_block_length: 1,
        max_block_length: MAX_BLOCK_SIZE,
    });
    let instance = unsafe { plugin.instantiate(features.clone(), SAMPLE_RATE)? };
    println!("Instantiated: {}", plugin.name());
    let instance_handle = instance.raw().instance().handle();

    let request = LaunchUiRequest {
        plugin: &plugin,
        features,
        instance_handle,
        port_write: Box::new(|message| {
            println!("{message:?}");
        }),
    };

    let mut handle = live_keys_lv2_ui::launch_ui(request)?;
    println!("UI launched, should be visible now. Waiting 10s...");
    std::thread::sleep(std::time::Duration::from_secs(10));
    println!("closed by user? {}", handle.is_closed());

    handle.request_close();
    println!("done");
    Ok(())
}
