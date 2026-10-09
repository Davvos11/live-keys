//! Manual test: opens a VST3 plugin's editor and pumps it for a while.
//! `cargo run -p live-keys-vst3-ui --example open_editor --release -- [name] [seconds]`
//! (`name` is a case-insensitive substring of the plugin name; default "surge xt").

use std::time::{Duration, Instant};

use live_keys_vst3_host::Vst3Host;
use live_keys_vst3_ui::Vst3Editor;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "surge xt".into()).to_lowercase();
    let seconds: u64 = args.next().map_or(Ok(10), |s| s.parse())?;

    let host = Vst3Host::new(512, &[]);
    let plugins = host.list_plugins();
    let info = plugins
        .iter()
        .find(|info| info.name.to_lowercase() == name)
        .or_else(|| plugins.iter().find(|info| info.name.to_lowercase().contains(&name)))
        .ok_or_else(|| anyhow::anyhow!("no VST3 plugin matching '{name}'"))?;
    let (_plugin, controller) = host.instantiate(&info.id, 48_000.0)?;
    let mut editor = Vst3Editor::open(
        &controller,
        &info.name,
        Box::new(|id, value| println!("edit: param {id} -> {value:.3}")),
    )?;
    println!("editor open for {}", info.name);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && !editor.is_closed() {
        editor.idle();
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("closed by user: {}", editor.is_closed());
    editor.close();
    println!("done");
    Ok(())
}
