//! Opens a CLAP plugin's GUI (name substring, exact match preferred) for N
//! seconds: `open_gui "surge xt" 10`.

use live_keys_clap_host::ClapHost;
use live_keys_clap_ui::ClapEditor;
use live_keys_core::HostedPlugin;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "surge xt".into()).to_lowercase();
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let host = ClapHost::new(512, &[]);
    let plugins = host.list_plugins();
    let info = plugins
        .iter()
        .find(|p| p.name.to_lowercase() == name)
        .or_else(|| plugins.iter().find(|p| p.name.to_lowercase().contains(&name)))
        .ok_or_else(|| anyhow::anyhow!("no CLAP plugin named {name}"))?;
    let (mut plugin, controller) = host.instantiate(&info.id, 48_000.0)?;
    let mut editor = ClapEditor::open(&controller, &info.name)?;
    println!("opened {} (window {:?})", info.name, editor.window());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline && !editor.is_closed() {
        plugin.process(&[], 512);
        let _ = controller.idle(|id, value| println!("edit {id} = {value}"));
        editor.idle();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    editor.close();
    Ok(())
}
