use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{Context, Result};
use livi::lilv::ui::UI;
use lv2_raw::LV2Feature;
use lv2_raw::ui::LV2UIIdleInterface;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::suil_sys::{self, SuilController};
use crate::{EVENT_TRANSFER_URI, LaunchUiRequest, PluginUiHandle, PortMessage};

const X11_UI_URI: &CStr = c"http://lv2plug.in/ns/extensions/ui#X11UI";
const UI_PARENT_URI: &CStr = c"http://lv2plug.in/ns/extensions/ui#parent";
const IDLE_INTERFACE_URI: &CStr = c"http://lv2plug.in/ns/extensions/ui#idleInterface";
const INSTANCE_ACCESS_URI: &CStr = c"http://lv2plug.in/ns/ext/instance-access";
const RESIZE_URI: &CStr = c"http://lv2plug.in/ns/extensions/ui#resize";

/// `LV2UI_Resize` (not in `lv2_raw`): offered by the host so the UI can ask for
/// a size, and by a UI so the host can tell it its new size.
#[repr(C)]
struct Lv2UiResize {
    handle: *mut c_void,
    ui_resize: extern "C" fn(handle: *mut c_void, width: c_int, height: c_int) -> c_int,
}

/// Records a UI's size request (handle: a `Cell<Option<(u32, u32)>>`), applied
/// by the hosting loop — this may be called while the UI is being created,
/// before there's a widget to resize.
extern "C" fn request_resize(handle: *mut c_void, width: c_int, height: c_int) -> c_int {
    if width <= 0 || height <= 0 {
        return 1;
    }
    // SAFETY: `handle` is the hosting thread's `Cell`, alive as long as the UI.
    let requested = unsafe { &*(handle as *const std::cell::Cell<Option<(u32, u32)>>) };
    requested.set(Some((width as u32, height as u32)));
    0
}
/// Fills the slot `livi::Features::iter_features` needs for a "worker" feature,
/// which UIs never need. A private URI guarantees nothing will ever match it.
const UNUSED_FEATURE_URI: &CStr = c"urn:live-keys:unused-feature";

static SUIL_INIT: Once = Once::new();

/// `suil_init` should run before any GUI toolkit code, per its own docs. Harmless
/// to call more than once in principle, but `Once` keeps it to exactly once anyway.
fn ensure_suil_init() {
    SUIL_INIT.call_once(|| {
        let mut argc: c_int = 0;
        let mut argv: *mut *mut c_char = std::ptr::null_mut();
        // SAFETY: matches suil's documented call shape (SUIL_ARG_NONE, no further
        // varargs); argc/argv are dummies since we're not suil's real entry point.
        unsafe { suil_sys::suil_init(&mut argc, &mut argv, 0) };
    });
}

/// A raw pointer, asserted `Send` for moving into the dedicated hosting thread. Only
/// ever touched from that one thread after this point.
struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}

/// These X11 requests are all best-effort cleanup/cosmetic calls where there's
/// nothing more to do on failure (either we're already tearing down and reporting a
/// different error, or a resize just stays cosmetically wrong) — but a failure here,
/// especially "the connection is already gone", is still worth a debug log rather
/// than disappearing silently.
fn log_if_err<T, E: std::fmt::Display>(result: Result<T, E>, what: &str) {
    if let Err(err) = result {
        log::debug!("{what}: {err}");
    }
}

pub(crate) fn launch(request: LaunchUiRequest<'_>, ui: UI) -> Result<PluginUiHandle> {
    ensure_suil_init();

    let plugin_uri = request.plugin.uri();
    let plugin_name = request.plugin.name();
    let ui_uri = ui
        .uri()
        .as_uri()
        .context("plugin UI has no URI")?
        .to_string();
    let ui_type = ui
        .classes()
        .iter()
        .find_map(|c| c.as_uri().map(str::to_string))
        .context("plugin UI has no class")?;
    let ui_type_c_check = CString::new(ui_type.clone())?;
    // SAFETY: both pointers are valid, live CStrings for the duration of this call.
    let support_quality =
        unsafe { suil_sys::suil_ui_supported(X11_UI_URI.as_ptr(), ui_type_c_check.as_ptr()) };
    if support_quality == 0 {
        anyhow::bail!("suil cannot wrap this plugin's '{ui_type}' UI as X11");
    }
    let (_, mut bundle_path) = ui
        .bundle_uri()
        .and_then(|node| node.path())
        .context("plugin UI bundle has no filesystem path")?;
    if !bundle_path.ends_with('/') {
        bundle_path.push('/');
    }
    let (_, binary_path) = ui
        .binary_uri()
        .and_then(|node| node.path())
        .context("plugin UI binary has no filesystem path")?;
    let port_symbols: Vec<(String, u32)> = request
        .plugin
        .ports()
        .map(|port| (port.symbol, port.index.0 as u32))
        .collect();

    let features = request.features;
    let instance_ptr = SendPtr(request.instance_handle);
    let port_write = request.port_write;

    let (close_tx, close_rx) = mpsc::channel::<()>();
    let (notify_tx, notify_rx) = mpsc::channel::<PortMessage>();
    let closed = Arc::new(AtomicBool::new(false));
    let closed_for_thread = closed.clone();

    // Intentionally not kept/joined — see `PluginUiHandle`'s docs for why.
    std::thread::Builder::new()
        .name("lv2-x11-ui".to_string())
        .spawn(move || {
            if let Err(err) = run(
                &plugin_uri,
                &plugin_name,
                &ui_uri,
                &ui_type,
                &bundle_path,
                &binary_path,
                &features,
                instance_ptr,
                port_write,
                port_symbols,
                &closed_for_thread,
                &close_rx,
                &notify_rx,
            ) {
                log::error!("X11 UI hosting failed: {err:#}");
            }
            closed_for_thread.store(true, Ordering::SeqCst);
        })
        .context("failed to spawn X11 UI hosting thread")?;

    Ok(PluginUiHandle::new(close_tx, notify_tx, closed))
}

#[allow(clippy::too_many_arguments)]
fn run(
    plugin_uri: &str,
    plugin_name: &str,
    ui_uri: &str,
    ui_type: &str,
    bundle_path: &str,
    binary_path: &str,
    features: &Arc<livi::Features>,
    instance_ptr: SendPtr,
    port_write: Box<dyn Fn(PortMessage) + Send>,
    port_symbols: Vec<(String, u32)>,
    closed: &Arc<AtomicBool>,
    close_rx: &mpsc::Receiver<()>,
    notify_rx: &mpsc::Receiver<PortMessage>,
) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("failed to connect to X server")?;
    let screen = &conn.setup().roots[screen_num];
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        screen.root,
        0,
        0,
        800,
        600,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.white_pixel)
            .event_mask(EventMask::STRUCTURE_NOTIFY),
    )?;

    let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let wm_delete_window = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete_window],
    )?;
    let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
    let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        net_wm_name,
        utf8_string,
        plugin_name.as_bytes(),
    )?;
    conn.map_window(window)?;
    conn.flush()?;

    let plugin_uri_c = CString::new(plugin_uri)?;
    let ui_uri_c = CString::new(ui_uri)?;
    let ui_type_c = CString::new(ui_type)?;
    let bundle_path_c = CString::new(bundle_path)?;
    let binary_path_c = CString::new(binary_path)?;

    let unused_feature = LV2Feature {
        uri: UNUSED_FEATURE_URI.as_ptr(),
        data: std::ptr::null_mut(),
    };
    let instance_access_feature = LV2Feature {
        uri: INSTANCE_ACCESS_URI.as_ptr(),
        data: instance_ptr.0,
    };
    let parent_feature = LV2Feature {
        uri: UI_PARENT_URI.as_ptr(),
        data: window as usize as *mut c_void,
    };
    let requested_size = std::cell::Cell::new(None::<(u32, u32)>);
    let host_resize = Lv2UiResize {
        handle: std::ptr::from_ref(&requested_size) as *mut c_void,
        ui_resize: request_resize,
    };
    let resize_feature = LV2Feature {
        uri: RESIZE_URI.as_ptr(),
        data: std::ptr::from_ref(&host_resize) as *mut c_void,
    };
    let mut feature_ptrs: Vec<*const LV2Feature> = features
        .iter_features(&unused_feature)
        .map(std::ptr::from_ref)
        .collect();
    feature_ptrs.push(std::ptr::from_ref(&instance_access_feature));
    feature_ptrs.push(std::ptr::from_ref(&parent_feature));
    feature_ptrs.push(std::ptr::from_ref(&resize_feature));
    feature_ptrs.push(std::ptr::null());

    let event_transfer = features.urid(EVENT_TRANSFER_URI);
    let callbacks = Box::into_raw(Box::new(UiCallbacks {
        port_write,
        port_symbols,
        event_transfer,
    }));

    // SAFETY: FFI into suil, following its documented calling convention.
    let host = unsafe {
        suil_sys::suil_host_new(
            write_function,
            Some(port_index_function),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if host.is_null() {
        drop(unsafe { Box::from_raw(callbacks) });
        log_if_err(conn.destroy_window(window), "destroy_window after suil_host_new failure");
        anyhow::bail!("suil_host_new failed");
    }

    // SAFETY: all pointers passed are valid, live CStrings/arrays for the duration
    // of this call, per suil_instance_new's documented contract.
    let instance = unsafe {
        suil_sys::suil_instance_new(
            host,
            callbacks as SuilController,
            X11_UI_URI.as_ptr(),
            plugin_uri_c.as_ptr(),
            ui_uri_c.as_ptr(),
            ui_type_c.as_ptr(),
            bundle_path_c.as_ptr(),
            binary_path_c.as_ptr(),
            feature_ptrs.as_ptr(),
        )
    };
    if instance.is_null() {
        unsafe { suil_sys::suil_host_free(host) };
        drop(unsafe { Box::from_raw(callbacks) });
        log_if_err(conn.destroy_window(window), "destroy_window after suil_instance_new failure");
        anyhow::bail!("suil failed to instantiate this plugin's UI");
    }

    // SAFETY: instance is non-null, per the check above.
    let widget = unsafe { suil_sys::suil_instance_get_widget(instance) };
    if widget.is_null() {
        unsafe { suil_sys::suil_instance_free(instance) };
        unsafe { suil_sys::suil_host_free(host) };
        drop(unsafe { Box::from_raw(callbacks) });
        log_if_err(conn.destroy_window(window), "destroy_window after missing widget");
        anyhow::bail!("plugin UI returned no widget");
    }
    let widget_xid = widget as usize as u32;
    // Idempotent even if suil already parented the widget under `window` itself.
    conn.reparent_window(widget_xid, window, 0, 0)?;
    conn.map_window(widget_xid)?;
    conn.flush()?;

    // Fit our window to the size the UI asked for (`ui:resize`), else to the
    // widget's own geometry, rather than leaving it at the initial 800x600.
    let initial_size = requested_size.take().or_else(|| {
        let geometry = conn.get_geometry(widget_xid).ok()?.reply().ok()?;
        Some((u32::from(geometry.width), u32::from(geometry.height)))
    });
    if let Some((width, height)) = initial_size {
        resize(&conn, window, width, height);
    }

    // SAFETY: instance is non-null and stays alive until suil_instance_free below.
    let idle_interface = unsafe {
        suil_sys::suil_instance_extension_data(instance, IDLE_INTERFACE_URI.as_ptr())
    } as *const LV2UIIdleInterface;
    let ui_handle = unsafe { suil_sys::suil_instance_get_handle(instance) };
    // The UI's side of `ui:resize`, if it has one: how it learns its new size
    // when the user resizes our window.
    // SAFETY: as for `idle_interface`.
    let ui_resize = unsafe { suil_sys::suil_instance_extension_data(instance, RESIZE_URI.as_ptr()) }
        as *const Lv2UiResize;
    let mut widget_size = initial_size;

    while !closed.load(Ordering::SeqCst) && close_rx.try_recv().is_err() {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::ClientMessage(msg) => {
                    let data = msg.data.as_data32();
                    if msg.window == window && data[0] == wm_delete_window {
                        closed.store(true, Ordering::SeqCst);
                    }
                }
                // The user resized our window: the embedded widget follows.
                Event::ConfigureNotify(configure) if configure.window == window => {
                    let size = (u32::from(configure.width), u32::from(configure.height));
                    if widget_size != Some(size) {
                        widget_size = Some(size);
                        resize(&conn, widget_xid, size.0, size.1);
                        if !ui_resize.is_null() {
                            // The UI's own interface takes its UI handle, like the
                            // idle interface — not the struct's `handle`, which UIs
                            // leave unset (JUCE's is null: Surge XT crashed on it).
                            // SAFETY: valid while `instance` is, like `idle_interface`.
                            unsafe { ((*ui_resize).ui_resize)(ui_handle, size.0 as c_int, size.1 as c_int) };
                        }
                    }
                }
                _ => {}
            }
        }
        // The UI asked for a new size: our window follows, and the widget with it
        // (via the ConfigureNotify above).
        if let Some((width, height)) = requested_size.take() {
            resize(&conn, window, width, height);
        }
        if closed.load(Ordering::SeqCst) {
            break;
        }
        while let Ok(message) = notify_rx.try_recv() {
            let (port, format, bytes): (u32, u32, &[u8]) = match &message {
                PortMessage::Control { port, value } => (*port, 0, bytemuck_f32(value)),
                PortMessage::Atom { port, atom } => (*port, event_transfer, atom),
            };
            // SAFETY: instance is live; suil only reads `bytes` during the call.
            unsafe {
                suil_sys::suil_instance_port_event(
                    instance,
                    port,
                    bytes.len() as u32,
                    format,
                    bytes.as_ptr().cast(),
                )
            };
        }
        if !idle_interface.is_null() {
            // SAFETY: idle_interface is non-null and, per suil's contract, valid
            // for as long as `instance` is; ui_handle is the matching UI handle.
            let should_close = unsafe { ((*idle_interface).idle)(ui_handle) } != 0;
            if should_close {
                closed.store(true, Ordering::SeqCst);
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    unsafe { suil_sys::suil_instance_free(instance) };
    unsafe { suil_sys::suil_host_free(host) };
    // SAFETY: nothing references `callbacks` anymore now that the UI is torn down.
    drop(unsafe { Box::from_raw(callbacks) });
    log_if_err(conn.destroy_window(window), "destroy_window during teardown");
    log_if_err(conn.flush(), "flush during teardown");

    Ok(())
}

fn resize(conn: &impl Connection, window: u32, width: u32, height: u32) {
    log_if_err(
        conn.configure_window(
            window,
            &x11rb::protocol::xproto::ConfigureWindowAux::new().width(width).height(height),
        ),
        "configure_window",
    );
    log_if_err(conn.flush(), "flush after resize");
}

struct UiCallbacks {
    port_write: Box<dyn Fn(PortMessage) + Send>,
    port_symbols: Vec<(String, u32)>,
    event_transfer: u32,
}

/// A float's bytes, as `port_event` takes a control value.
fn bytemuck_f32(value: &f32) -> &[u8] {
    // SAFETY: any f32 is 4 initialized bytes; the slice borrows `value`.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast(), size_of::<f32>()) }
}

extern "C" fn write_function(
    controller: SuilController,
    port_index: u32,
    buffer_size: u32,
    protocol: u32,
    buffer: *const c_void,
) {
    // SAFETY: controller is the pointer we handed to suil_instance_new, a live
    // `UiCallbacks` for as long as the UI instance exists.
    let callbacks = unsafe { &*(controller as *const UiCallbacks) };
    if let Some(message) =
        unsafe { crate::port_message(port_index, buffer_size, protocol, buffer, callbacks.event_transfer) }
    {
        (callbacks.port_write)(message);
    }
}

extern "C" fn port_index_function(controller: SuilController, port_symbol: *const c_char) -> u32 {
    // SAFETY: same as write_function's controller.
    let callbacks = unsafe { &*(controller as *const UiCallbacks) };
    // SAFETY: suil always passes a valid, NUL-terminated C string here.
    let Ok(symbol) = (unsafe { CStr::from_ptr(port_symbol) }).to_str() else {
        return u32::MAX;
    };
    callbacks
        .port_symbols
        .iter()
        .find(|(sym, _)| sym == symbol)
        .map_or(u32::MAX, |(_, index)| *index)
}
