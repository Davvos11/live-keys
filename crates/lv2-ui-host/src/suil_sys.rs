//! Minimal hand-written bindings to the subset of `suil`'s C API we need. No Rust
//! crate exists for `suil`, so this exists in place of a `suil-sys` crate — see
//! <https://gitlab.com/lv2/suil/-/blob/master/include/suil/suil.h> for the source of
//! truth these are transcribed from.
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void};

use lv2_raw::LV2Feature;

pub type SuilController = *mut c_void;
pub type SuilHandle = *mut c_void;
pub type SuilWidget = *mut c_void;

pub type SuilPortWriteFunc = extern "C" fn(
    controller: SuilController,
    port_index: u32,
    buffer_size: u32,
    protocol: u32,
    buffer: *const c_void,
);

pub type SuilPortIndexFunc =
    extern "C" fn(controller: SuilController, port_symbol: *const c_char) -> u32;

#[repr(C)]
pub struct SuilHost {
    _private: [u8; 0],
}

#[repr(C)]
pub struct SuilInstance {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// `void suil_init(int* argc, char*** argv, SuilArg key, ...)` — variadic in C,
    /// but we only ever call it with the required `SUIL_ARG_NONE` terminator and no
    /// further arguments, which is a valid call for a C variadic function.
    pub fn suil_init(argc: *mut c_int, argv: *mut *mut *mut c_char, key: c_int, ...);

    pub fn suil_ui_supported(host_type_uri: *const c_char, ui_type_uri: *const c_char) -> u32;

    pub fn suil_host_new(
        write_func: SuilPortWriteFunc,
        index_func: Option<SuilPortIndexFunc>,
        subscribe_func: *const c_void,
        unsubscribe_func: *const c_void,
    ) -> *mut SuilHost;

    pub fn suil_host_free(host: *mut SuilHost);

    #[allow(clippy::too_many_arguments)]
    pub fn suil_instance_new(
        host: *mut SuilHost,
        controller: SuilController,
        container_type_uri: *const c_char,
        plugin_uri: *const c_char,
        ui_uri: *const c_char,
        ui_type_uri: *const c_char,
        ui_bundle_path: *const c_char,
        ui_binary_path: *const c_char,
        features: *const *const LV2Feature,
    ) -> *mut SuilInstance;

    pub fn suil_instance_free(instance: *mut SuilInstance);

    pub fn suil_instance_get_handle(instance: *mut SuilInstance) -> SuilHandle;

    pub fn suil_instance_get_widget(instance: *mut SuilInstance) -> SuilWidget;

    pub fn suil_instance_port_event(
        instance: *mut SuilInstance,
        port_index: u32,
        buffer_size: u32,
        format: u32,
        buffer: *const c_void,
    );

    pub fn suil_instance_extension_data(
        instance: *mut SuilInstance,
        uri: *const c_char,
    ) -> *const c_void;
}
