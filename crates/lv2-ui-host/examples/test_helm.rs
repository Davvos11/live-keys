//! Manual smoke test for the External UI hosting path: instantiates Helm and opens
//! its own GUI window for a few seconds. Run with `cargo run --example test_helm`.

use live_keys_core::HostedPlugin;
use live_keys_lv2_host::Lv2Host;
use live_keys_lv2_ui::LaunchUiRequest;

const HELM_URI: &str = "http://tytel.org/helm";

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let host = Lv2Host::new(8192);
    let plugin = host.instantiate(HELM_URI, 48000.0)?;
    println!("Instantiated: {}", plugin.name());

    let request = LaunchUiRequest {
        plugin: plugin.plugin(),
        features: plugin.features().clone(),
        instance_handle: plugin.raw_handle(),
        port_write: Box::new(|port_index, value| {
            println!("port {port_index} -> {value}");
        }),
    };

    let mut handle = live_keys_lv2_ui::launch_ui(request)?;
    println!("UI launched, should be visible now. Waiting 8s...");
    std::thread::sleep(std::time::Duration::from_secs(8));
    println!("closed by user? {}", handle.is_closed());

    handle.request_close();
    println!("done");
    Ok(())
}
