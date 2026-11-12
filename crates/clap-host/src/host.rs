//! The host side a plugin sees: one `clap_host` per plugin instance, with the
//! host extensions it can ask for.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_host_audio_ports};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::log::*;
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_host_note_ports,
    clap_note_dialect,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_VALUES, clap_host_params,
    clap_param_clear_flags, clap_param_rescan_flags,
};
use clap_sys::ext::posix_fd_support::{
    CLAP_EXT_POSIX_FD_SUPPORT, clap_host_posix_fd_support, clap_posix_fd_flags,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use clap_sys::version::CLAP_VERSION;

thread_local! {
    /// Set while this thread runs a plugin's audio-thread functions
    /// (`start_processing`, `process`, `stop_processing`), for `thread-check`.
    static IN_AUDIO: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` with this thread counted as the audio thread.
pub(crate) fn as_audio_thread<T>(f: impl FnOnce() -> T) -> T {
    let was = IN_AUDIO.replace(true);
    let result = f();
    IN_AUDIO.set(was);
    result
}

pub(crate) struct Timer {
    pub id: clap_id,
    pub interval: Duration,
    pub due: Instant,
}

/// What the plugin's GUI asked of the host, for the editor to act on.
#[derive(Default)]
pub(crate) struct GuiRequests {
    /// A new size for the window (`request_resize`), from any thread.
    pub resize: Option<(u32, u32)>,
    /// A floating window was closed by the user, or the GUI was destroyed.
    pub closed: bool,
}

/// One plugin instance's host: the `clap_host` it was created with (whose
/// `host_data` points back here) and everything the plugin registers with it.
/// Boxed, so the pointers the plugin keeps stay valid.
pub(crate) struct HostShared {
    pub clap_host: clap_host,
    /// For log messages.
    pub name: String,
    main_thread: ThreadId,
    /// Set once the plugin exists; needed to call it back (timers, fds).
    pub plugin: AtomicPtr<clap_plugin>,
    /// The plugin asked for `on_main_thread`.
    pub callback_requested: AtomicBool,
    /// The plugin said its parameter values changed (`params.rescan`).
    pub values_changed: AtomicBool,
    pub timers: Mutex<Vec<Timer>>,
    next_timer_id: AtomicU32,
    /// Registered fds and the events they're watched for.
    pub fds: Mutex<Vec<(i32, clap_posix_fd_flags)>>,
    pub gui: Mutex<GuiRequests>,
}

const HOST_NAME: &CStr = c"Live Keys";
const HOST_VENDOR: &CStr = c"Live Keys";
const HOST_URL: &CStr = c"https://github.com/Davvos11/live-keys";
const HOST_VERSION: &CStr = c"0.1.0";

impl HostShared {
    /// A new host for a plugin created on this (the main) thread.
    pub fn new(name: String) -> Box<Self> {
        let mut host = Box::new(Self {
            clap_host: clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: HOST_NAME.as_ptr(),
                vendor: HOST_VENDOR.as_ptr(),
                url: HOST_URL.as_ptr(),
                version: HOST_VERSION.as_ptr(),
                get_extension: Some(get_extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            },
            name,
            main_thread: std::thread::current().id(),
            plugin: AtomicPtr::new(std::ptr::null_mut()),
            callback_requested: AtomicBool::new(false),
            values_changed: AtomicBool::new(false),
            timers: Mutex::new(Vec::new()),
            next_timer_id: AtomicU32::new(1),
            fds: Mutex::new(Vec::new()),
            gui: Mutex::new(GuiRequests::default()),
        });
        host.clap_host.host_data = &*host as *const Self as *mut c_void;
        host
    }

    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread
    }
}

/// The `HostShared` behind a `clap_host` pointer the plugin passed back.
unsafe fn shared<'a>(host: *const clap_host) -> Option<&'a HostShared> {
    unsafe { (host.as_ref()?.host_data as *const HostShared).as_ref() }
}

unsafe extern "C" fn get_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let id = unsafe { CStr::from_ptr(id) };
    let ext: *const c_void = if id == CLAP_EXT_THREAD_CHECK {
        &THREAD_CHECK as *const _ as _
    } else if id == CLAP_EXT_LOG {
        &LOG as *const _ as _
    } else if id == CLAP_EXT_PARAMS {
        &PARAMS as *const _ as _
    } else if id == CLAP_EXT_GUI {
        &GUI as *const _ as _
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        &TIMER_SUPPORT as *const _ as _
    } else if id == CLAP_EXT_POSIX_FD_SUPPORT {
        &POSIX_FD_SUPPORT as *const _ as _
    } else if id == CLAP_EXT_STATE {
        &STATE as *const _ as _
    } else if id == CLAP_EXT_NOTE_PORTS {
        &NOTE_PORTS as *const _ as _
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &AUDIO_PORTS as *const _ as _
    } else if id == CLAP_EXT_LATENCY {
        &LATENCY as *const _ as _
    } else {
        std::ptr::null()
    };
    ext
}

/// Asking for a deactivate/reactivate cycle (new latency, ports…) isn't acted on
/// yet; see "Known simplifications".
unsafe extern "C" fn request_restart(host: *const clap_host) {
    if let Some(host) = unsafe { shared(host) } {
        log::debug!("CLAP plugin '{}' asked for a restart, which isn't supported", host.name);
    }
}

/// Plugins are processed all the time anyway.
unsafe extern "C" fn request_process(_host: *const clap_host) {}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    if let Some(host) = unsafe { shared(host) } {
        host.callback_requested.store(true, Ordering::Release);
    }
}

static THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(is_main_thread),
    is_audio_thread: Some(is_audio_thread),
};

unsafe extern "C" fn is_main_thread(host: *const clap_host) -> bool {
    unsafe { shared(host) }.is_some_and(HostShared::is_main_thread)
}

unsafe extern "C" fn is_audio_thread(_host: *const clap_host) -> bool {
    IN_AUDIO.get()
}

static LOG: clap_host_log = clap_host_log { log: Some(log_message) };

unsafe extern "C" fn log_message(host: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    let (Some(host), false) = (unsafe { shared(host) }, msg.is_null()) else {
        return;
    };
    let msg = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    let name = &host.name;
    match severity {
        CLAP_LOG_DEBUG => log::debug!("{name}: {msg}"),
        CLAP_LOG_INFO => log::info!("{name}: {msg}"),
        CLAP_LOG_WARNING => log::warn!("{name}: {msg}"),
        CLAP_LOG_HOST_MISBEHAVING => log::warn!("{name} says the host misbehaves: {msg}"),
        CLAP_LOG_PLUGIN_MISBEHAVING => log::warn!("{name} misbehaves: {msg}"),
        _ => log::error!("{name}: {msg}"),
    }
}

/// Parameter metadata is read once at load, and plugins always process, so
/// there's nothing to flush.
static PARAMS: clap_host_params = clap_host_params {
    rescan: Some(params_rescan),
    clear: Some(params_clear),
    request_flush: Some(params_request_flush),
};

/// New values are read back by the controller's next `idle`; new parameters or
/// metadata (`CLAP_PARAM_RESCAN_INFO`) aren't picked up — see "Known simplifications".
unsafe extern "C" fn params_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    if let Some(host) = unsafe { shared(host) }
        && flags & (CLAP_PARAM_RESCAN_VALUES | CLAP_PARAM_RESCAN_ALL) != 0
    {
        host.values_changed.store(true, Ordering::Release);
    }
}
unsafe extern "C" fn params_clear(_host: *const clap_host, _id: clap_id, _flags: clap_param_clear_flags) {}
unsafe extern "C" fn params_request_flush(_host: *const clap_host) {}

static GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(gui_resize_hints_changed),
    request_resize: Some(gui_request_resize),
    request_show: Some(gui_request_show),
    request_hide: Some(gui_request_hide),
    closed: Some(gui_closed),
};

unsafe extern "C" fn gui_resize_hints_changed(_host: *const clap_host) {}

unsafe extern "C" fn gui_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    let Some(host) = (unsafe { shared(host) }) else { return false };
    host.gui.lock().unwrap().resize = Some((width, height));
    true
}

unsafe extern "C" fn gui_request_show(_host: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn gui_request_hide(_host: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn gui_closed(host: *const clap_host, _was_destroyed: bool) {
    if let Some(host) = unsafe { shared(host) } {
        host.gui.lock().unwrap().closed = true;
    }
}

static TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(register_timer),
    unregister_timer: Some(unregister_timer),
};

unsafe extern "C" fn register_timer(host: *const clap_host, period_ms: u32, timer_id: *mut clap_id) -> bool {
    let (Some(host), false) = (unsafe { shared(host) }, timer_id.is_null()) else {
        return false;
    };
    let id = host.next_timer_id.fetch_add(1, Ordering::Relaxed);
    let interval = Duration::from_millis(u64::from(period_ms.max(1)));
    host.timers.lock().unwrap().push(Timer { id, interval, due: Instant::now() + interval });
    unsafe { *timer_id = id };
    true
}

unsafe extern "C" fn unregister_timer(host: *const clap_host, timer_id: clap_id) -> bool {
    let Some(host) = (unsafe { shared(host) }) else { return false };
    let mut timers = host.timers.lock().unwrap();
    let before = timers.len();
    timers.retain(|timer| timer.id != timer_id);
    timers.len() != before
}

static POSIX_FD_SUPPORT: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(register_fd),
    modify_fd: Some(modify_fd),
    unregister_fd: Some(unregister_fd),
};

unsafe extern "C" fn register_fd(host: *const clap_host, fd: i32, flags: clap_posix_fd_flags) -> bool {
    let Some(host) = (unsafe { shared(host) }) else { return false };
    let mut fds = host.fds.lock().unwrap();
    if fds.iter().any(|&(registered, _)| registered == fd) {
        return false;
    }
    fds.push((fd, flags));
    true
}

unsafe extern "C" fn modify_fd(host: *const clap_host, fd: i32, flags: clap_posix_fd_flags) -> bool {
    let Some(host) = (unsafe { shared(host) }) else { return false };
    let mut fds = host.fds.lock().unwrap();
    match fds.iter_mut().find(|(registered, _)| *registered == fd) {
        Some(entry) => {
            entry.1 = flags;
            true
        }
        None => false,
    }
}

unsafe extern "C" fn unregister_fd(host: *const clap_host, fd: i32) -> bool {
    let Some(host) = (unsafe { shared(host) }) else { return false };
    let mut fds = host.fds.lock().unwrap();
    let before = fds.len();
    fds.retain(|&(registered, _)| registered != fd);
    fds.len() != before
}

/// State is saved whenever the app saves (session, presets), so being told it
/// changed isn't needed.
static STATE: clap_host_state = clap_host_state { mark_dirty: Some(state_mark_dirty) };

unsafe extern "C" fn state_mark_dirty(_host: *const clap_host) {}

static NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(note_dialects),
    rescan: Some(note_ports_rescan),
};

unsafe extern "C" fn note_dialects(_host: *const clap_host) -> clap_note_dialect {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

unsafe extern "C" fn note_ports_rescan(_host: *const clap_host, _flags: u32) {}

static AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(audio_ports_rescan_supported),
    rescan: Some(audio_ports_rescan),
};

unsafe extern "C" fn audio_ports_rescan_supported(_host: *const clap_host, _flag: u32) -> bool {
    false
}

unsafe extern "C" fn audio_ports_rescan(_host: *const clap_host, _flags: u32) {}

/// There's no latency compensation (see "Known simplifications").
static LATENCY: clap_host_latency = clap_host_latency { changed: Some(latency_changed) };

unsafe extern "C" fn latency_changed(_host: *const clap_host) {}
