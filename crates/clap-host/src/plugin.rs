//! A running CLAP plugin, split into the half the audio thread owns
//! ([`ClapPlugin`]) and the half the main thread owns ([`ClapController`]).

use std::ffi::{CStr, CString, c_char};
use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports};
use clap_sys::ext::gui::{CLAP_EXT_GUI, CLAP_WINDOW_API_X11, clap_plugin_gui, clap_window, clap_window_handle};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_note_port_info,
    clap_plugin_note_ports,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_BYPASS, CLAP_PARAM_IS_ENUM, CLAP_PARAM_IS_HIDDEN,
    CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED, clap_param_info, clap_plugin_params,
};
use clap_sys::ext::posix_fd_support::{
    CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
    clap_plugin_posix_fd_support,
};
use clap_sys::ext::preset_load::{CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT, clap_plugin_preset_load};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_plugin_timer_support};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{CLAP_PROCESS_ERROR, clap_process};
use live_keys_core::{
    Consumer, HostedPlugin, MidiEvent, ParamChange, ParamChoice, ParamInfo, ParamKind, Producer,
    param_ring_buffer,
};

use crate::events::{InputEvents, OutputEvents};
use crate::host::{HostShared, as_audio_thread};
use crate::module::{Module, char_array};
use crate::state;

/// Most events one block can carry (MIDI and parameter changes); extra ones in a
/// dense burst are dropped.
const EVENT_CAPACITY: usize = 1024;
/// Parameter edits the plugin made itself (its GUI) waiting for the main thread.
const EDIT_RING_CAPACITY: usize = 1024;
/// Most options a stepped enum parameter is shown as a choice for.
const MAX_CHOICES: i64 = 256;
/// How long after loading a state its values are read back again: some plugins
/// (Surge XT) apply a loaded state on their next audio block.
const STATE_SETTLE: Duration = Duration::from_millis(100);

/// Plugin instances whose last owner went away off the main thread, waiting for
/// [`destroy_pending`] to tear them down there (`deactivate`/`destroy` are
/// main-thread functions).
static PENDING_DESTROY: Mutex<Vec<RawInstance>> = Mutex::new(Vec::new());

/// Tears down plugin instances let go of on other threads (e.g. with an audio
/// stream dropped on a thread of its own). Main thread only.
pub fn destroy_pending() {
    let pending = std::mem::take(&mut *PENDING_DESTROY.lock().unwrap());
    for raw in pending {
        raw.destroy();
    }
}

struct RawInstance {
    plugin: *const clap_plugin,
    activated: bool,
    host: Box<HostShared>,
    _module: Arc<Module>,
}

// SAFETY: a plugin may be handed between threads; each call into it is made on
// the thread the CLAP threading rules name for it.
unsafe impl Send for RawInstance {}

impl RawInstance {
    fn destroy(self) {
        unsafe {
            let plugin = &*self.plugin;
            if self.activated
                && let Some(deactivate) = plugin.deactivate
            {
                deactivate(self.plugin);
            }
            if let Some(destroy) = plugin.destroy {
                destroy(self.plugin);
            }
        }
        // The host struct must outlive the plugin, which may call it until the end.
        drop(self.host);
    }
}

/// The plugin instance both halves share, torn down once neither is left.
pub(crate) struct Instance {
    raw: Option<RawInstance>,
}

// SAFETY: see `RawInstance`.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    fn raw(&self) -> &RawInstance {
        self.raw.as_ref().unwrap()
    }

    pub(crate) fn host(&self) -> &HostShared {
        &self.raw().host
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let Some(raw) = self.raw.take() else { return };
        if raw.host.is_main_thread() {
            raw.destroy();
        } else {
            PENDING_DESTROY.lock().unwrap().push(raw);
        }
    }
}

/// Extension `ids` (tried in order) of `plugin`, valid as long as the plugin is.
unsafe fn extension<T>(plugin: *const clap_plugin, ids: &[&CStr]) -> Option<&'static T> {
    let get = unsafe { (*plugin).get_extension? };
    ids.iter()
        .find_map(|id| unsafe { (get(plugin, id.as_ptr()) as *const T).as_ref() })
}

/// Channel counts of the plugin's audio ports in one direction.
fn audio_ports(plugin: *const clap_plugin, ext: Option<&clap_plugin_audio_ports>, input: bool) -> Vec<usize> {
    let Some((count, get)) = ext.and_then(|ext| Some((ext.count?, ext.get?))) else {
        return Vec::new();
    };
    (0..unsafe { count(plugin, input) })
        .map(|index| {
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            if unsafe { get(plugin, index, input, &mut info) } {
                info.channel_count as usize
            } else {
                0
            }
        })
        .collect()
}

/// The dialects the plugin's first note port in one direction supports, if it
/// has one.
fn note_port_dialects(plugin: *const clap_plugin, ext: Option<&clap_plugin_note_ports>, input: bool) -> Option<u32> {
    let (count, get) = ext.and_then(|ext| Some((ext.count?, ext.get?)))?;
    if unsafe { count(plugin, input) } == 0 {
        return None;
    }
    let mut info: clap_note_port_info = unsafe { std::mem::zeroed() };
    unsafe { get(plugin, 0, input, &mut info) }.then_some(info.supported_dialects)
}

/// The plugin's text for parameter `id` at `value` ("-6.0 dB", "Saw").
fn value_text(plugin: *const clap_plugin, ext: &clap_plugin_params, id: u32, value: f64) -> Option<String> {
    let to_text = ext.value_to_text?;
    let mut buffer = [0 as c_char; 256];
    unsafe { to_text(plugin, id, value, buffer.as_mut_ptr(), buffer.len() as u32) }
        .then(|| char_array(&buffer))
        .filter(|text| !text.is_empty())
}

/// The parameters worth showing in a parameter editor (not hidden, not
/// read-only), in plain values.
fn read_params(plugin: *const clap_plugin, ext: Option<&clap_plugin_params>) -> Vec<ParamInfo> {
    let Some(ext) = ext else { return Vec::new() };
    let (Some(count), Some(get_info)) = (ext.count, ext.get_info) else {
        return Vec::new();
    };
    (0..unsafe { count(plugin) })
        .filter_map(|index| {
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            if !unsafe { get_info(plugin, index, &mut info) }
                || info.flags & (CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_READONLY) != 0
            {
                return None;
            }
            let (min, max) = (info.min_value, info.max_value);
            if !(min.is_finite() && max.is_finite()) || max <= min {
                return None;
            }
            let mut param = ParamInfo::continuous(
                info.id,
                char_array(&info.name),
                info.default_value.clamp(min, max) as f32,
                min as f32,
                max as f32,
            );
            let span = (max - min).round() as i64;
            if info.flags & CLAP_PARAM_IS_BYPASS != 0 {
                param.kind = ParamKind::Toggle;
            } else if info.flags & CLAP_PARAM_IS_STEPPED != 0 {
                if info.flags & CLAP_PARAM_IS_ENUM != 0 && (1..=MAX_CHOICES).contains(&span) {
                    // The plugin names its own options.
                    param.kind = ParamKind::Choice;
                    param.choices = (0..=span)
                        .map(|step| {
                            let value = min + step as f64;
                            ParamChoice {
                                value: value as f32,
                                label: value_text(plugin, ext, info.id, value)
                                    .unwrap_or_else(|| step.to_string()),
                            }
                        })
                        .collect();
                } else if span == 1 {
                    param.kind = ParamKind::Toggle;
                } else if span > 1 {
                    param.kind = ParamKind::Stepped;
                    param.steps = span as u32;
                }
            }
            Some(param)
        })
        .collect()
}

/// Creates plugin `plugin_id` from `module` and activates it at
/// `sample_rate`/`max_block_size`. Main thread only.
pub(crate) fn instantiate(
    module: Arc<Module>,
    plugin_id: &str,
    name: String,
    sample_rate: f64,
    max_block_size: usize,
) -> Result<(ClapPlugin, ClapController)> {
    let host = HostShared::new(name.clone());
    let create = module.factory.create_plugin.context("the factory can't create plugins")?;
    let id = CString::new(plugin_id)?;
    let plugin = unsafe { create(module.factory, &host.clap_host, id.as_ptr()) };
    anyhow::ensure!(!plugin.is_null(), "the factory couldn't create the plugin");
    host.plugin.store(plugin as *mut clap_plugin, std::sync::atomic::Ordering::Release);
    // From here on, dropping `instance` tears down whatever's been set up.
    let mut instance = Instance {
        raw: Some(RawInstance { plugin, activated: false, host, _module: module }),
    };
    let funcs = unsafe { &*plugin };
    anyhow::ensure!(
        funcs.init.is_some_and(|init| unsafe { init(plugin) }),
        "the plugin failed to initialize"
    );

    let params_ext: Option<&clap_plugin_params> = unsafe { extension(plugin, &[CLAP_EXT_PARAMS]) };
    let audio_ports_ext = unsafe { extension(plugin, &[CLAP_EXT_AUDIO_PORTS]) };
    let note_ports_ext = unsafe { extension(plugin, &[CLAP_EXT_NOTE_PORTS]) };
    let output_channels = audio_ports(plugin, audio_ports_ext, false);
    let input_channels = audio_ports(plugin, audio_ports_ext, true);
    let note_input = note_port_dialects(plugin, note_ports_ext, true);
    log::debug!(
        "{name}: audio outputs {output_channels:?}, audio inputs {input_channels:?}, note input dialects {note_input:?}"
    );
    let params = read_params(plugin, params_ext);

    let activate = funcs.activate.context("the plugin can't be activated")?;
    anyhow::ensure!(
        unsafe { activate(plugin, sample_rate, 1, max_block_size as u32) },
        "the plugin failed to activate ({sample_rate} Hz, up to {max_block_size} frames)"
    );
    instance.raw.as_mut().unwrap().activated = true;
    let instance = Arc::new(instance);

    let (edit_tx, edit_rx) = param_ring_buffer(EDIT_RING_CAPACITY);
    let dialects = note_input.unwrap_or(0);
    let audio = ClapPlugin::new(
        name,
        instance.clone(),
        &output_channels,
        &input_channels,
        NoteInput {
            clap_notes: dialects & CLAP_NOTE_DIALECT_CLAP != 0,
            raw_midi: dialects & CLAP_NOTE_DIALECT_MIDI != 0,
        },
        edit_tx,
        max_block_size,
    );
    let controller = ClapController {
        plugin,
        params,
        params_ext,
        state_ext: unsafe { extension(plugin, &[CLAP_EXT_STATE]) },
        gui_ext: unsafe { extension(plugin, &[CLAP_EXT_GUI]) },
        timer_ext: unsafe { extension(plugin, &[CLAP_EXT_TIMER_SUPPORT]) },
        fd_ext: unsafe { extension(plugin, &[CLAP_EXT_POSIX_FD_SUPPORT]) },
        preset_load_ext: unsafe { extension(plugin, &[CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT]) },
        edit_rx: Mutex::new(edit_rx),
        reread_at: Cell::new(None),
        instance,
    };
    Ok((audio, controller))
}

/// How MIDI reaches the plugin's note input.
#[derive(Clone, Copy)]
struct NoteInput {
    /// Notes as CLAP note events.
    clap_notes: bool,
    /// Everything else (and notes, without `clap_notes`) as raw MIDI events.
    raw_midi: bool,
}

/// The audio-thread half of a running CLAP plugin. Every buffer and event list
/// is allocated at load time.
pub struct ClapPlugin {
    name: String,
    plugin: *const clap_plugin,
    /// Per output port, per channel. Only port 0's are read back.
    output_buffers: Vec<Vec<Vec<f32>>>,
    /// Input port 0's channels, filled by the host.
    input_buffers: Vec<Vec<f32>>,
    /// Fed to every channel of input ports past the first (e.g. a sidechain).
    silence: Vec<f32>,
    _output_ptrs: Vec<Vec<*mut f32>>,
    _input_ptrs: Vec<Vec<*mut f32>>,
    output_ports: Vec<clap_audio_buffer>,
    input_ports: Vec<clap_audio_buffer>,
    note_input: NoteInput,
    in_events: InputEvents,
    out_events: OutputEvents,
    /// The plugin's own parameter changes, for the main thread.
    edit_tx: Producer<ParamChange>,
    steady_time: i64,
    frames_rendered: usize,
    /// `start_processing` has been called (lazily, on the audio thread).
    started: bool,
    /// Something failed and was logged once: stays silent from then on.
    failed: bool,
    /// Dropped last, after processing has been stopped in `drop`.
    _instance: Arc<Instance>,
}

// SAFETY: the raw pointers point into this struct's own heap allocations or at
// the plugin it owns, which is only called from whichever thread currently owns
// this half, as the audio thread.
unsafe impl Send for ClapPlugin {}

impl ClapPlugin {
    fn new(
        name: String,
        instance: Arc<Instance>,
        output_channels: &[usize],
        input_channels: &[usize],
        note_input: NoteInput,
        edit_tx: Producer<ParamChange>,
        max_block_size: usize,
    ) -> Self {
        let mut output_buffers: Vec<Vec<Vec<f32>>> = output_channels
            .iter()
            .map(|&channels| vec![vec![0.0; max_block_size]; channels])
            .collect();
        let mut input_buffers = vec![vec![0.0; max_block_size]; input_channels.first().copied().unwrap_or(0)];
        let mut silence = vec![0.0; max_block_size];
        let output_ptrs: Vec<Vec<*mut f32>> = output_buffers
            .iter_mut()
            .map(|port| port.iter_mut().map(|channel| channel.as_mut_ptr()).collect())
            .collect();
        let silence_ptr = silence.as_mut_ptr();
        let input_ptrs: Vec<Vec<*mut f32>> = input_channels
            .iter()
            .enumerate()
            .map(|(port, &channels)| match port {
                0 => input_buffers.iter_mut().map(|channel| channel.as_mut_ptr()).collect(),
                _ => vec![silence_ptr; channels],
            })
            .collect();
        let ports = |ptrs: &Vec<Vec<*mut f32>>| -> Vec<clap_audio_buffer> {
            ptrs.iter()
                .map(|channels| clap_audio_buffer {
                    data32: channels.as_ptr() as *mut *mut f32,
                    data64: std::ptr::null_mut(),
                    channel_count: channels.len() as u32,
                    latency: 0,
                    constant_mask: 0,
                })
                .collect()
        };
        Self {
            name,
            plugin: instance.raw().plugin,
            output_ports: ports(&output_ptrs),
            input_ports: ports(&input_ptrs),
            output_buffers,
            input_buffers,
            silence,
            _output_ptrs: output_ptrs,
            _input_ptrs: input_ptrs,
            note_input,
            in_events: InputEvents::new(EVENT_CAPACITY),
            out_events: OutputEvents::new(EVENT_CAPACITY),
            edit_tx,
            steady_time: 0,
            frames_rendered: 0,
            started: false,
            failed: false,
            _instance: instance,
        }
    }

    fn fail(&mut self, what: &str) {
        if !self.failed {
            // Logged once, not every block: a rare error path on the audio thread.
            self.failed = true;
            log::error!("CLAP plugin '{}' {what}", self.name);
        }
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        if self.started
            && let Some(stop) = unsafe { (*self.plugin).stop_processing }
        {
            // Whoever drops this half is the last to process it.
            as_audio_thread(|| unsafe { stop(self.plugin) });
        }
    }
}

impl HostedPlugin for ClapPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn audio_output_count(&self) -> usize {
        self.output_buffers.first().map_or(0, Vec::len)
    }

    fn audio_input_count(&self) -> usize {
        self.input_buffers.len()
    }

    fn audio_input_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.input_buffers[channel]
    }

    fn process(&mut self, midi_in: &[MidiEvent], frames: usize) {
        let NoteInput { clap_notes, raw_midi } = self.note_input;
        let last_frame = frames.saturating_sub(1) as u32;
        for event in midi_in {
            self.in_events.push_midi(event, event.frame_offset.min(last_frame), clap_notes, raw_midi);
        }
        self.silence[..frames].fill(0.0);
        self.frames_rendered = frames;
        self.out_events.clear();

        if !self.started && !self.failed {
            let start = unsafe { (*self.plugin).start_processing };
            self.started = start.is_some_and(|start| as_audio_thread(|| unsafe { start(self.plugin) }));
            if !self.started {
                self.fail("failed to start processing");
            }
        }
        let status = if self.started && !self.failed {
            let process = unsafe { (*self.plugin).process };
            let in_events = self.in_events.as_clap();
            let out_events = self.out_events.as_clap();
            let data = clap_process {
                steady_time: self.steady_time,
                frames_count: frames as u32,
                transport: std::ptr::null(),
                audio_inputs: self.input_ports.as_ptr(),
                audio_outputs: self.output_ports.as_mut_ptr(),
                audio_inputs_count: self.input_ports.len() as u32,
                audio_outputs_count: self.output_ports.len() as u32,
                in_events: &in_events,
                out_events: &out_events,
            };
            process.map_or(CLAP_PROCESS_ERROR, |process| as_audio_thread(|| unsafe { process(self.plugin, &data) }))
        } else {
            CLAP_PROCESS_ERROR
        };
        self.in_events.clear();
        self.steady_time += frames as i64;
        if status == CLAP_PROCESS_ERROR {
            if self.started {
                self.fail("failed to process");
            }
            if let Some(port) = self.output_buffers.first_mut() {
                for channel in port {
                    channel[..frames].fill(0.0);
                }
            }
            self.out_events.clear();
        }
        for &(param_id, value) in &self.out_events.params {
            // A full ring means the main thread isn't reading; dropping is all
            // there is to do here.
            let _ = self.edit_tx.push(ParamChange { param_id, value: value as f32 });
        }
    }

    fn audio_output(&self, channel: usize) -> &[f32] {
        &self.output_buffers[0][channel][..self.frames_rendered]
    }

    fn midi_output(&self) -> &[MidiEvent] {
        &self.out_events.midi
    }

    fn set_param(&mut self, id: u32, value: f32) {
        self.in_events.push_param(id, f64::from(value));
    }
}

/// Where a preset found by preset discovery is (see `preset.rs`).
pub(crate) struct PresetLocation<'a> {
    pub kind: u32,
    pub location: Option<&'a str>,
    pub load_key: Option<&'a str>,
}

/// The main-thread half of a running CLAP plugin: everything but processing —
/// parameters' metadata and text, state, presets, the GUI, and the plugin's
/// main-thread callbacks, timers and fds (see [`ClapController::idle`]). Every
/// call must come from the main thread.
pub struct ClapController {
    plugin: *const clap_plugin,
    params: Vec<ParamInfo>,
    params_ext: Option<&'static clap_plugin_params>,
    state_ext: Option<&'static clap_plugin_state>,
    gui_ext: Option<&'static clap_plugin_gui>,
    timer_ext: Option<&'static clap_plugin_timer_support>,
    fd_ext: Option<&'static clap_plugin_posix_fd_support>,
    preset_load_ext: Option<&'static clap_plugin_preset_load>,
    edit_rx: Mutex<Consumer<ParamChange>>,
    /// When to read every value back after a state load (see [`STATE_SETTLE`]).
    reread_at: Cell<Option<Instant>>,
    instance: Arc<Instance>,
}

impl ClapController {
    /// The plugin's visible, writable parameters, in plain values.
    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// The plugin's own text for parameter `id` at `value`, unit included.
    pub fn value_text(&self, id: u32, value: f32) -> Option<String> {
        value_text(self.plugin, self.params_ext?, id, f64::from(value))
    }

    /// The plugin's current value of parameter `id`.
    pub fn param_value(&self, id: u32) -> Option<f32> {
        let get = self.params_ext?.get_value?;
        let mut value = 0.0;
        unsafe { get(self.plugin, id, &mut value) }.then_some(value as f32)
    }

    /// Does what the plugin asked the main thread for since the last call: its
    /// `on_main_thread` callback, due timers and ready fds. Hands `on_edit` the
    /// parameter changes the plugin made itself (in its GUI) — already applied
    /// by the plugin, so they're only to be recorded, not sent back. Call
    /// regularly (every ~16 ms); many plugins' GUIs run on these timers.
    ///
    /// `true` if any parameter values may have changed otherwise (the plugin
    /// said so, or a state load has settled), so they should be read again with
    /// [`ClapController::param_value`].
    pub fn idle(&self, mut on_edit: impl FnMut(u32, f32)) -> bool {
        let host = self.instance.host();
        if host.callback_requested.swap(false, std::sync::atomic::Ordering::AcqRel)
            && let Some(callback) = unsafe { (*self.plugin).on_main_thread }
        {
            unsafe { callback(self.plugin) };
        }
        self.dispatch_timers(host);
        self.dispatch_fds(host);
        let mut edits = self.edit_rx.lock().unwrap();
        while let Ok(change) = edits.pop() {
            on_edit(change.param_id, change.value);
        }
        let settled = self.reread_at.get().is_some_and(|at| at <= Instant::now());
        if settled {
            self.reread_at.set(None);
        }
        host.values_changed.swap(false, std::sync::atomic::Ordering::AcqRel) || settled
    }

    fn dispatch_timers(&self, host: &HostShared) {
        let Some(on_timer) = self.timer_ext.and_then(|ext| ext.on_timer) else { return };
        let now = Instant::now();
        // Collected first: a timer callback may (un)register timers.
        let due: Vec<u32> = host
            .timers
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|timer| timer.due <= now)
            .map(|timer| {
                // Skip missed ticks rather than firing a burst to catch up.
                timer.due = now + timer.interval;
                timer.id
            })
            .collect();
        for id in due {
            if host.timers.lock().unwrap().iter().any(|timer| timer.id == id) {
                unsafe { on_timer(self.plugin, id) };
            }
        }
    }

    fn dispatch_fds(&self, host: &HostShared) {
        let Some(on_fd) = self.fd_ext.and_then(|ext| ext.on_fd) else { return };
        let fds: Vec<(i32, u32)> = host.fds.lock().unwrap().clone();
        if fds.is_empty() {
            return;
        }
        let mut polled: Vec<libc::pollfd> = fds
            .iter()
            .map(|&(fd, flags)| libc::pollfd {
                fd,
                events: if flags & CLAP_POSIX_FD_READ != 0 { libc::POLLIN } else { 0 }
                    | if flags & CLAP_POSIX_FD_WRITE != 0 { libc::POLLOUT } else { 0 },
                revents: 0,
            })
            .collect();
        if unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, 0) } <= 0 {
            return;
        }
        for entry in polled {
            let mut flags = 0;
            if entry.revents & libc::POLLIN != 0 {
                flags |= CLAP_POSIX_FD_READ;
            }
            if entry.revents & libc::POLLOUT != 0 {
                flags |= CLAP_POSIX_FD_WRITE;
            }
            if entry.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                flags |= CLAP_POSIX_FD_ERROR;
            }
            let registered = host.fds.lock().unwrap().iter().any(|&(fd, _)| fd == entry.fd);
            if flags != 0 && registered {
                unsafe { on_fd(self.plugin, entry.fd, flags) };
            }
        }
    }

    /// The plugin's state, as it saves it. Allowed while it processes.
    pub fn state(&self) -> Result<Vec<u8>> {
        let save = self.state_ext.and_then(|ext| ext.save).context("this plugin can't save its state")?;
        let mut buffer = Vec::new();
        let stream = state::ostream(&mut buffer);
        anyhow::ensure!(unsafe { save(self.plugin, &stream) }, "the plugin didn't save its state");
        Ok(buffer)
    }

    /// Restores state saved by [`ClapController::state`]. Allowed while it
    /// processes.
    pub fn set_state(&self, bytes: &[u8]) -> Result<()> {
        let load = self.state_ext.and_then(|ext| ext.load).context("this plugin can't load a state")?;
        let mut reader = state::Reader::new(bytes);
        let stream = reader.istream();
        anyhow::ensure!(unsafe { load(self.plugin, &stream) }, "the plugin rejected the state");
        self.reread_at.set(Some(Instant::now() + STATE_SETTLE));
        Ok(())
    }

    /// Loads a preset found by preset discovery.
    pub(crate) fn load_preset(&self, preset: &PresetLocation) -> Result<()> {
        let load = self
            .preset_load_ext
            .and_then(|ext| ext.from_location)
            .context("this plugin can't load presets")?;
        let location = preset.location.map(CString::new).transpose()?;
        let load_key = preset.load_key.map(CString::new).transpose()?;
        let ptr = |s: &Option<CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        anyhow::ensure!(
            unsafe { load(self.plugin, preset.kind, ptr(&location), ptr(&load_key)) },
            "the plugin couldn't load the preset"
        );
        self.reread_at.set(Some(Instant::now() + STATE_SETTLE));
        Ok(())
    }

    /// Whether the plugin has a GUI this host can show (an X11 one).
    pub fn has_gui(&self) -> bool {
        self.gui().is_some_and(|gui| gui.is_api_supported(false) || gui.is_api_supported(true))
    }

    /// The plugin's GUI extension, for hosting its GUI (see `live-keys-clap-ui`).
    pub fn gui(&self) -> Option<ClapGui> {
        Some(ClapGui {
            plugin: self.plugin,
            ext: self.gui_ext?,
            _instance: self.instance.clone(),
        })
    }
}

/// A plugin's `clap.gui` extension, with X11 as the window API. Main thread
/// only. Keeps the plugin alive while held.
pub struct ClapGui {
    plugin: *const clap_plugin,
    ext: &'static clap_plugin_gui,
    _instance: Arc<Instance>,
}

impl ClapGui {
    pub fn is_api_supported(&self, floating: bool) -> bool {
        self.ext
            .is_api_supported
            .is_some_and(|f| unsafe { f(self.plugin, CLAP_WINDOW_API_X11.as_ptr(), floating) })
    }

    pub fn create(&self, floating: bool) -> bool {
        self.ext
            .create
            .is_some_and(|f| unsafe { f(self.plugin, CLAP_WINDOW_API_X11.as_ptr(), floating) })
    }

    pub fn destroy(&self) {
        if let Some(f) = self.ext.destroy {
            unsafe { f(self.plugin) };
        }
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        let f = self.ext.get_size?;
        let (mut width, mut height) = (0, 0);
        (unsafe { f(self.plugin, &mut width, &mut height) } && width > 0 && height > 0).then_some((width, height))
    }

    pub fn can_resize(&self) -> bool {
        self.ext.can_resize.is_some_and(|f| unsafe { f(self.plugin) })
    }

    /// The nearest size to `size` the GUI can take.
    pub fn adjust_size(&self, (mut width, mut height): (u32, u32)) -> (u32, u32) {
        if let Some(f) = self.ext.adjust_size {
            unsafe { f(self.plugin, &mut width, &mut height) };
        }
        (width, height)
    }

    pub fn set_size(&self, (width, height): (u32, u32)) -> bool {
        self.ext.set_size.is_some_and(|f| unsafe { f(self.plugin, width, height) })
    }

    /// Embeds the GUI into X11 window `window`.
    pub fn set_parent(&self, window: u32) -> bool {
        let window = clap_window {
            api: CLAP_WINDOW_API_X11.as_ptr(),
            specific: clap_window_handle { x11: window.into() },
        };
        self.ext.set_parent.is_some_and(|f| unsafe { f(self.plugin, &window) })
    }

    pub fn suggest_title(&self, title: &str) {
        if let (Some(f), Ok(title)) = (self.ext.suggest_title, CString::new(title)) {
            unsafe { f(self.plugin, title.as_ptr()) };
        }
    }

    pub fn show(&self) -> bool {
        self.ext.show.is_some_and(|f| unsafe { f(self.plugin) })
    }

    pub fn hide(&self) {
        if let Some(f) = self.ext.hide {
            unsafe { f(self.plugin) };
        }
    }

    /// A size the plugin asked its window to be (`request_resize`), once.
    pub fn take_resize_request(&self) -> Option<(u32, u32)> {
        self._instance.host().gui.lock().unwrap().resize.take()
    }

    /// Whether the plugin said its (floating) window was closed, once.
    pub fn take_closed(&self) -> bool {
        std::mem::take(&mut self._instance.host().gui.lock().unwrap().closed)
    }
}
