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

use crate::{LaunchUiRequest, PluginUiHandle};

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
    let closed = Arc::new(AtomicBool::new(false));
    let closed_for_thread = closed.clone();

    let join_handle = std::thread::Builder::new()
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
            ) {
                log::error!("external UI hosting failed: {err:#}");
            }
            closed_for_thread.store(true, Ordering::SeqCst);
        })
        .context("failed to spawn external UI hosting thread")?;

    Ok(PluginUiHandle::new(close_tx, join_handle, closed))
}

#[allow(clippy::too_many_arguments)]
fn run(
    plugin_uri: &str,
    ui_uri: &str,
    bundle_path: &str,
    binary_path: &str,
    features: &Arc<livi::Features>,
    instance_ptr: SendPtr,
    port_write: Box<dyn Fn(u32, f32) + Send>,
    closed: &Arc<AtomicBool>,
    close_rx: &mpsc::Receiver<()>,
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
        ui_closed,
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

    let callbacks = Box::into_raw(Box::new(UiCallbacks {
        port_write,
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

    while !closed.load(Ordering::SeqCst) && close_rx.try_recv().is_err() {
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
    port_write: Box<dyn Fn(u32, f32) + Send>,
    closed: Arc<AtomicBool>,
}

extern "C" fn write_function(
    controller: LV2UIControllerRaw,
    port_index: u32,
    buffer_size: u32,
    port_protocol: u32,
    buffer: *const c_void,
) {
    if port_protocol != 0 || buffer.is_null() || buffer_size as usize != size_of::<f32>() {
        return;
    }
    // SAFETY: controller is the pointer we handed to instantiate_raw, a live
    // `UiCallbacks` for as long as the UI instance exists.
    let callbacks = unsafe { &*(controller as *const UiCallbacks) };
    // SAFETY: checked above that buffer is non-null and sized for one f32, per the
    // "format 0" contract of LV2UI_Write_Function.
    let value = unsafe { *(buffer as *const f32) };
    (callbacks.port_write)(port_index, value);
}

extern "C" fn ui_closed(host: LV2UIControllerRaw) -> c_void {
    // SAFETY: same as write_function's controller.
    let callbacks = unsafe { &*(host as *const UiCallbacks) };
    callbacks.closed.store(true, Ordering::SeqCst);
    unreachable!("LV2_External_UI_Host::ui_closed has no meaningful return value")
}
