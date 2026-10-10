//! Manual smoke test for LV2 presets: lists a plugin's presets, applies each,
//! then saves a user preset, finds it in the list, and deletes it again.
//! `cargo run -p live-keys-lv2-host --example presets -- <plugin uri>`

use live_keys_core::HostedPlugin;
use live_keys_lv2_host::Lv2Host;

fn main() -> anyhow::Result<()> {
    let uri = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://drobilla.net/plugins/mda/EPiano".to_string());
    let host = Lv2Host::new(1024);
    let mut plugin = host.instantiate(&uri, 48000.0)?;
    let instance = plugin.instance_ref();
    println!("{} — has state: {}", plugin.name(), instance.has_state());
    let presets = host.presets(&uri);
    for preset in &presets {
        let values = unsafe { host.restore_preset(instance, &uri, &preset.id)? };
        println!("  {:30} user={} {} port values  {}", preset.name, preset.user, values.len(), preset.id);
        for (index, value) in values {
            plugin.set_param(index, value);
        }
    }
    let defaults: Vec<(u32, f32)> = plugin.params().iter().map(|p| (p.id, p.default_value)).collect();
    let value = |index: u32| defaults.iter().find(|(id, _)| *id == index).map_or(0.0, |(_, v)| *v);
    let saved = unsafe { host.save_preset(instance, &uri, plugin.name(), "Smoke Test", &value)? };
    println!("saved {saved:?}");
    let listed = host.presets(&uri).into_iter().find(|p| p.id == saved.id);
    println!("listed after save: {listed:?}");
    let values = unsafe { host.restore_preset(instance, &uri, &saved.id)? };
    println!("restored saved preset: {} port values", values.len());
    host.delete_preset(&saved.id)?;
    println!("still listed after delete: {}", host.presets(&uri).iter().any(|p| p.id == saved.id));
    Ok(())
}
