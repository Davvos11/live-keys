mod discovery;
mod external_ui;
mod suil_sys;
mod x11_ui;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

pub use discovery::{UiKind, best_supported_ui};

/// A handle to a plugin's currently open GUI window, hosted on a dedicated
/// background thread (never the audio thread, never the GUI's own event-loop
/// thread).
///
/// Deliberately never joins that thread. `closed` flips true as soon as the plugin
/// itself reports its window closed (see `ui_closed`/the X11UI idle loop), which can
/// be well before the hosting thread finishes its own teardown — a plugin's
/// `cleanup()` may need to keep pumping its internal message loop a little longer,
/// and occasionally (observed with Helm, intermittently, after interacting with its
/// UI) that teardown stalls. Blocking the caller — typically the GUI's main thread,
/// dropping the previous handle the moment it notices `is_closed()` so the user can
/// reopen — on a `join()` here would freeze the whole GUI waiting for a plugin's own
/// (buggy) cleanup code. Instead the hosting thread is left to finish, or hang, on
/// its own; a hang leaks that one OS thread, which is a far better outcome than
/// freezing the app.
pub struct PluginUiHandle {
    close_tx: mpsc::Sender<()>,
    closed: Arc<AtomicBool>,
}

impl PluginUiHandle {
    fn new(close_tx: mpsc::Sender<()>, closed: Arc<AtomicBool>) -> Self {
        Self { close_tx, closed }
    }

    /// True once the UI has closed, whether the user closed the window themselves
    /// or [`Self::request_close`] was called. Note this can be true slightly before
    /// the hosting thread has finished tearing down (see the type's docs).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Asks the hosting thread to close the UI. Does not wait for it to finish —
    /// see the type's docs for why. A no-op if already closed.
    pub fn request_close(&mut self) {
        // Fails only if the hosting thread already exited on its own (e.g. the user
        // already closed the window) — nothing to signal in that case.
        let _ = self.close_tx.send(());
    }
}

impl Drop for PluginUiHandle {
    fn drop(&mut self) {
        self.request_close();
    }
}

/// Everything needed to launch a plugin's own GUI, independent of how it's hosted.
pub struct LaunchUiRequest<'a> {
    pub plugin: &'a livi::Plugin,
    pub features: Arc<livi::Features>,
    pub instance_handle: lv2_raw::core::LV2Handle,
    /// Called (from the hosting thread) whenever the plugin's own GUI writes a new
    /// value to one of its control ports, e.g. the user turning a knob. Must be
    /// safe to call from a thread other than wherever `launch_ui` was called from.
    pub port_write: Box<dyn Fn(u32, f32) + Send>,
}

/// Discovers and launches the best UI this host can show for `request.plugin`.
/// Returns `Err` if the plugin has no UI this host knows how to drive, or if
/// launching it failed.
pub fn launch_ui(request: LaunchUiRequest<'_>) -> anyhow::Result<PluginUiHandle> {
    let (ui, kind) = discovery::best_supported_ui(request.plugin)
        .ok_or_else(|| anyhow::anyhow!("this plugin has no GUI this host can display"))?;

    match kind {
        UiKind::ExternalUi => external_ui::launch(request, ui),
        UiKind::X11Ui => x11_ui::launch(request, ui),
    }
}
