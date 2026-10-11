use std::ffi::{CStr, CString, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use livi::lilv::ui::UI;
use lv2_raw::LV2Feature;
use lv2_raw::ui::{
    LV2UIControllerRaw, LV2UIDescriptorRaw, LV2UIExternalUIHost, LV2UIExternalUIWidget,
    LV2UIWidget,
};

use crate::{EVENT_TRANSFER_URI, LaunchUiRequest, PluginUiHandle, PortMessage};

const INSTANCE_ACCESS_URI: &CStr = c"http://lv2plug.in/ns/ext/instance-access";
const EXTERNAL_UI_HOST_URI: &CStr = c"http://kxstudio.sf.net/ns/lv2ext/external-ui#Host";
/// Fills the slot `livi::Features::iter_features` needs for a "worker" feature,
/// which UIs never need. A private URI guarantees nothing will ever match it.
const UNUSED_FEATURE_URI: &CStr = c"urn:live-keys:unused-feature";

/// A raw pointer, asserted `Send` for moving into the dedicated hosting thread. Only
/// ever handed off, never touched from more than one thread at a time.
struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}

pub(crate) fn launch(request: LaunchUiRequest<'_>, ui: UI) -> Result<PluginUiHandle> {
    let plugin_uri = request.plugin.uri();
    let ui_uri = ui
        .uri()
        .as_uri()
        .context("plugin UI has no URI")?
        .to_string();
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

    let features = request.features;
    let instance_ptr = SendPtr(request.instance_handle);
    let port_write = request.port_write;

    let (close_tx, close_rx) = mpsc::channel::<()>();
    let (notify_tx, notify_rx) = mpsc::channel::<PortMessage>();
    let closed = Arc::new(AtomicBool::new(false));
    let closed_for_thread = closed.clone();

    // Intentionally not kept/joined — see `PluginUiHandle`'s docs for why.
    std::thread::Builder::new()
        .name("lv2-external-ui".to_string())
        .spawn(move || {
            if let Err(err) = run(
                &plugin_uri,
                &ui_uri,
                &bundle_path,
                &binary_path,
                &features,
                instance_ptr,
                port_write,
                &closed_for_thread,
                &close_rx,
                &notify_rx,
            ) {
                log::error!("external UI hosting failed: {err:#}");
            }
            closed_for_thread.store(true, Ordering::SeqCst);
        })
        .context("failed to spawn external UI hosting thread")?;

    Ok(PluginUiHandle::new(close_tx, notify_tx, closed))
}

#[allow(clippy::too_many_arguments)]
fn run(
    plugin_uri: &str,
    ui_uri: &str,
    bundle_path: &str,
    binary_path: &str,
    features: &Arc<livi::Features>,
    instance_ptr: SendPtr,
    port_write: Box<dyn Fn(PortMessage) + Send>,
    closed: &Arc<AtomicBool>,
    close_rx: &mpsc::Receiver<()>,
    notify_rx: &mpsc::Receiver<PortMessage>,
) -> Result<()> {
    // SAFETY: dlopen'ing a plugin's own UI binary. The library is kept alive for
    // this whole function (dropped last), so descriptor/instantiate/cleanup
    // pointers borrowed from it stay valid throughout.
    let library = unsafe { libloading::Library::new(binary_path) }
        .with_context(|| format!("failed to load UI binary '{binary_path}'"))?;
    let descriptor_ptr = find_descriptor(&library, ui_uri, binary_path)?;
    // SAFETY: descriptor_ptr was just returned by the UI binary's own
    // lv2ui_descriptor() and is non-null (checked in find_descriptor).
    let descriptor = unsafe { &*descriptor_ptr };

    let plugin_uri_c = CString::new(plugin_uri)?;
    let bundle_path_c = CString::new(bundle_path)?;

    let unused_feature = LV2Feature {
        uri: UNUSED_FEATURE_URI.as_ptr(),
        data: std::ptr::null_mut(),
    };
    let instance_access_feature = LV2Feature {
        uri: INSTANCE_ACCESS_URI.as_ptr(),
        data: instance_ptr.0,
    };
    let external_ui_host = LV2UIExternalUIHost {
        // SAFETY: `lv2_raw`'s binding types this field as `extern "C" fn(...) ->
        // libc::c_void`, but `c_void` has no legally constructible value in Rust —
        // it's a mistranscription of a true C `void`-returning function. Function
        // pointers that differ only in a `void`/`()` return are ABI-compatible on
        // every platform we target, so transmuting the *pointer* (never a value)
        // bridges the two safely, without ever needing to "return" a `c_void`.
        ui_closed: unsafe {
            std::mem::transmute::<
                extern "C" fn(LV2UIControllerRaw),
                extern "C" fn(LV2UIControllerRaw) -> c_void,
            >(ui_closed)
        },
        plugin_human_id: std::ptr::null(),
    };
    let external_ui_host_feature = LV2Feature {
        uri: EXTERNAL_UI_HOST_URI.as_ptr(),
        data: std::ptr::from_ref(&external_ui_host) as *mut c_void,
    };

    let mut feature_ptrs: Vec<*const LV2Feature> = features
        .iter_features(&unused_feature)
        .map(std::ptr::from_ref)
        .collect();
    feature_ptrs.push(std::ptr::from_ref(&instance_access_feature));
    feature_ptrs.push(std::ptr::from_ref(&external_ui_host_feature));
    feature_ptrs.push(std::ptr::null());

    let event_transfer = features.urid(EVENT_TRANSFER_URI);
    let callbacks = Box::into_raw(Box::new(UiCallbacks {
        port_write,
        event_transfer,
        closed: closed.clone(),
    }));

    let mut widget: LV2UIWidget = std::ptr::null_mut();
    let handle = (descriptor.instantiate_raw)(
        descriptor_ptr,
        plugin_uri_c.as_ptr(),
        bundle_path_c.as_ptr(),
        Some(write_function),
        callbacks as LV2UIControllerRaw,
        &mut widget,
        feature_ptrs.as_ptr(),
    );

    if handle.is_null() || widget.is_null() {
        // SAFETY: callbacks was created by Box::into_raw above and hasn't been
        // freed yet; the UI never took ownership since instantiation failed.
        drop(unsafe { Box::from_raw(callbacks) });
        anyhow::bail!("plugin UI failed to instantiate");
    }
    let widget = widget as *mut LV2UIExternalUIWidget;

    // SAFETY: widget is non-null and, for an ExternalUI, points to a valid
    // LV2_External_UI_Widget for as long as the UI is instantiated (until
    // `cleanup` below).
    if let Some(show) = unsafe { (*widget).show } {
        show(widget);
    }

    // `lv2_raw` types `port_event` as a plain function pointer, but C allows it
    // to be NULL ("not interested in port events"); read it as the `Option` it is.
    // SAFETY: `Option<fn>` has the same layout as a nullable function pointer.
    let port_event = unsafe {
        std::ptr::addr_of!((*descriptor_ptr).port_event)
            .cast::<Option<PortEventFn>>()
            .read()
    };

    while !closed.load(Ordering::SeqCst) && close_rx.try_recv().is_err() {
        while let Ok(message) = notify_rx.try_recv() {
            let Some(port_event) = port_event else {
                continue;
            };
            let value;
            let (port, format, bytes): (u32, u32, &[u8]) = match &message {
                PortMessage::Control { port, value: v } => {
                    value = v.to_ne_bytes();
                    (*port, 0, &value)
                }
                PortMessage::Atom { port, atom } => (*port, event_transfer, atom),
            };
            // SAFETY: handle is the live UI; it only reads `bytes` during the call.
            port_event(handle, port, bytes.len() as u32, format, bytes.as_ptr().cast());
        }
        if let Some(run_fn) = unsafe { (*widget).run } {
            // May synchronously invoke `ui_closed` if the user closed the window.
            run_fn(widget);
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    if !closed.load(Ordering::SeqCst)
        && let Some(hide) = unsafe { (*widget).hide }
    {
        hide(widget);
    }
    (descriptor.cleanup)(handle);
    // SAFETY: nothing references `callbacks` anymore now that the UI is torn down.
    drop(unsafe { Box::from_raw(callbacks) });

    Ok(())
}

type PortEventFn = extern "C" fn(
    ui: lv2_raw::ui::LV2UIHandle,
    port_index: std::ffi::c_uint,
    buffer_size: std::ffi::c_uint,
    format: std::ffi::c_uint,
    buffer: *const c_void,
);

type Lv2UiDescriptorFn = unsafe extern "C" fn(index: u32) -> *const LV2UIDescriptorRaw;

fn find_descriptor(
    library: &libloading::Library,
    target_uri: &str,
    binary_path: &str,
) -> Result<*const LV2UIDescriptorRaw> {
    // SAFETY: `lv2ui_descriptor` is the standard LV2 UI entry point every
    // conforming UI binary exports with this exact signature.
    let lv2ui_descriptor: libloading::Symbol<Lv2UiDescriptorFn> =
        unsafe { library.get(b"lv2ui_descriptor\0") }
            .context("UI binary has no lv2ui_descriptor symbol")?;
    for index in 0.. {
        // SAFETY: calling into the UI binary's own descriptor enumeration
        // function, per the LV2 UI extension's calling convention.
        let descriptor = unsafe { lv2ui_descriptor(index) };
        if descriptor.is_null() {
            anyhow::bail!("no UI descriptor matching '{target_uri}' in '{binary_path}'");
        }
        // SAFETY: a non-null descriptor's `uri` field is a valid, NUL-terminated
        // C string for as long as the library stays loaded.
        let uri = unsafe { CStr::from_ptr((*descriptor).uri) };
        if uri.to_str() == Ok(target_uri) {
            return Ok(descriptor);
        }
    }
    unreachable!("0.. never ends")
}

struct UiCallbacks {
    port_write: Box<dyn Fn(PortMessage) + Send>,
    event_transfer: u32,
    closed: Arc<AtomicBool>,
}

extern "C" fn write_function(
    controller: LV2UIControllerRaw,
    port_index: u32,
    buffer_size: u32,
    port_protocol: u32,
    buffer: *const c_void,
) {
    // SAFETY: controller is the pointer we handed to instantiate_raw, a live
    // `UiCallbacks` for as long as the UI instance exists.
    let callbacks = unsafe { &*(controller as *const UiCallbacks) };
    // SAFETY: the UI passes `buffer_size` readable bytes, per LV2UI_Write_Function.
    if let Some(message) = unsafe {
        crate::port_message(port_index, buffer_size, port_protocol, buffer, callbacks.event_transfer)
    } {
        (callbacks.port_write)(message);
    }
}

/// Called by the plugin's own UI (often from its own toolkit's thread, not our
/// hosting thread) whenever the user closes its window — the normal, expected close
/// path, not a rare corner case. Must not panic: it's invoked across an `extern "C"`
/// boundary from arbitrary plugin code, and an unwind that tries to cross that
/// boundary aborts the whole process rather than propagate.
extern "C" fn ui_closed(host: LV2UIControllerRaw) {
    // SAFETY: same as write_function's controller.
    let callbacks = unsafe { &*(host as *const UiCallbacks) };
    callbacks.closed.store(true, Ordering::SeqCst);
}
