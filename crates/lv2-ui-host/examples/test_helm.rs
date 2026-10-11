//! Manual smoke test for the External UI hosting path: instantiates Helm and opens
//! its own GUI window twice in a row (close, then reopen), using the same running
//! plugin instance both times — mirroring the GUI's close/reopen workflow.
//! Run with `cargo run --example test_helm`, and close the Helm window yourself (or
//! send it a WM_DELETE_WINDOW via `send_wm_close`) within the wait window.

use std::time::{Duration, Instant};

use live_keys_core::HostedPlugin;
use live_keys_lv2_host::Lv2Host;
use live_keys_lv2_ui::LaunchUiRequest;

const HELM_URI: &str = "http://tytel.org/helm";
const WAIT_FOR_CLOSE: Duration = Duration::from_secs(15);
const ROUNDS: u32 = 3;

fn open_and_wait_for_close(
    plugin: &live_keys_lv2_host::Lv2Plugin,
    round: u32,
) -> anyhow::Result<()> {
    let request = LaunchUiRequest {
        plugin: plugin.plugin(),
        features: plugin.features().clone(),
        instance_handle: plugin.raw_handle(),
        port_write: Box::new(move |message| {
            println!("[round {round}] {message:?}");
        }),
        keep_alive: plugin.gui_leases().lease().map(|lease| Box::new(lease) as Box<dyn Send>),
    };
    let mut handle = live_keys_lv2_ui::launch_ui(request)?;
    println!("[round {round}] UI launched — close the window (up to {WAIT_FOR_CLOSE:?})");

    let start = Instant::now();
    while !handle.is_closed() && start.elapsed() < WAIT_FOR_CLOSE {
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "[round {round}] closed by user? {} (after {:?})",
        handle.is_closed(),
        start.elapsed()
    );
    handle.request_close();
    Ok(())
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let host = Lv2Host::new(8192);
    let plugin = host.instantiate(HELM_URI, 48000.0)?;
    println!("Instantiated: {}", plugin.name());

    for round in 1..=ROUNDS {
        open_and_wait_for_close(&plugin, round)?;
    }

    println!("done");
    Ok(())
}
