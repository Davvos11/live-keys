mod discovery;
mod external_ui;
mod suil_sys;
mod x11_ui;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;

pub use discovery::{UiKind, best_supported_ui};

/// `atom:eventTransfer`: the port protocol for atoms passed between a plugin and
/// its GUI.
const EVENT_TRANSFER_URI: &std::ffi::CStr = c"http://lv2plug.in/ns/ext/atom#eventTransfer";

/// Something passed between a plugin's DSP and its GUI through one of its ports.
#[derive(Debug, Clone, PartialEq)]
pub enum PortMessage {
    /// A control port's value (`ui:floatProtocol`).
    Control { port: u32, value: f32 },
    /// An atom for an atom port (`atom:eventTransfer`): its `LV2_Atom` header
    /// followed by its body. Plugins whose GUI talks to the DSP only this way
    /// (setBfree) have no control ports at all.
    Atom { port: u32, atom: Vec<u8> },
}

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
    notify_tx: mpsc::Sender<PortMessage>,
    closed: Arc<AtomicBool>,
    /// The X11 window an X11 UI is embedded in, once created (0 until then, and
    /// for an External UI, which owns its window).
    window: Arc<AtomicU32>,
}

impl PluginUiHandle {
    fn new(
        close_tx: mpsc::Sender<()>,
        notify_tx: mpsc::Sender<PortMessage>,
        closed: Arc<AtomicBool>,
        window: Arc<AtomicU32>,
    ) -> Self {
        Self {
            close_tx,
            notify_tx,
            closed,
            window,
        }
    }

    /// The X11 window an X11 UI is embedded in (for tests resizing it), once
    /// created; `None` before that and for an External UI.
    pub fn window(&self) -> Option<u32> {
        Some(self.window.load(Ordering::SeqCst)).filter(|&id| id != 0)
    }

    /// Tells the GUI about a port: a control port's new value (e.g. set from
    /// the host's parameter window or a preset), or an atom the plugin wrote to an
    /// output port the GUI listens to (`ui:portNotification`). Delivered on the
    /// hosting thread's next idle tick. A no-op once closed.
    pub fn notify(&self, message: PortMessage) {
        let _ = self.notify_tx.send(message);
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

/// What a GUI's `write_function` call wrote, if it's a protocol this host
/// handles: a float for a control port (format 0), or an atom (`event_transfer`,
/// the `atom:eventTransfer` URID).
///
/// # Safety
/// `buffer` must be null or point to `buffer_size` readable bytes.
unsafe fn port_message(
    port: u32,
    buffer_size: u32,
    format: u32,
    buffer: *const std::ffi::c_void,
    event_transfer: u32,
) -> Option<PortMessage> {
    if buffer.is_null() {
        return None;
    }
    if format == 0 && buffer_size as usize == size_of::<f32>() {
        // SAFETY: sized for one f32, per the check above and the caller.
        let value = unsafe { buffer.cast::<f32>().read_unaligned() };
        return Some(PortMessage::Control { port, value });
    }
    if format == event_transfer && buffer_size as usize >= 8 {
        // SAFETY: as the caller guarantees.
        let atom = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), buffer_size as usize) };
        return Some(PortMessage::Atom { port, atom: atom.to_vec() });
    }
    None
}

/// glibc's `RTLD_NODELETE` (not in `libloading`).
const RTLD_NODELETE: std::ffi::c_int = 0x1000;

/// Loads a plugin's UI binary for good: later `dlclose`s (suil's, or ours) leave
/// it mapped. Some UIs leave thread-local data with destructors in their own
/// code behind on the hosting thread (x42's, through the GLib they bundle), which
/// run when the thread exits — after the UI is freed. Unmapped by then, that
/// crashed the process. JUCE UIs don't survive unloading either (see the VST3
/// host's never-unload rule).
fn pin_library(binary_path: &str) -> anyhow::Result<()> {
    use libloading::os::unix::{Library, RTLD_NOW};
    // SAFETY: loading the plugin's own UI binary, as suil or the External UI
    // host is about to anyway; running its initializers is expected.
    let library = unsafe { Library::open(Some(binary_path), RTLD_NOW | RTLD_NODELETE) }
        .map_err(|err| anyhow::anyhow!("failed to load UI binary '{binary_path}': {err}"))?;
    std::mem::forget(library);
    Ok(())
}

/// Everything needed to launch a plugin's own GUI, independent of how it's hosted.
pub struct LaunchUiRequest<'a> {
    pub plugin: &'a livi::Plugin,
    pub features: Arc<livi::Features>,
    pub instance_handle: lv2_raw::core::LV2Handle,
    /// Called (from the hosting thread) whenever the plugin's own GUI writes to
    /// one of the plugin's ports: a control port's new value (e.g. the user turning
    /// a knob) or an atom for an atom input. Must be safe to call from a thread
    /// other than wherever `launch_ui` was called from.
    pub port_write: Box<dyn Fn(PortMessage) + Send>,
    /// Held by the hosting thread until the UI is completely torn down, then
    /// dropped — for keeping the DSP instance alive while a closing UI may still
    /// use it (`live_keys_lv2_host::GuiLease`).
    pub keep_alive: Option<Box<dyn Send>>,
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
