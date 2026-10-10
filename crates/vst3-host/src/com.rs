//! COM objects implemented on the host side and handed to a VST3 plugin: the host
//! context, the edit-callback sink, the per-block event and parameter-change
//! lists, and an in-memory stream for state transfer.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::ffi::{c_char, c_void};
use std::sync::Mutex;

use vst3::Steinberg::Vst::{
    Event, IComponentHandler, IComponentHandlerTrait, IEventList, IEventListTrait,
    IHostApplication, IHostApplicationTrait, IParamValueQueue, IParamValueQueueTrait,
    IParameterChanges, IParameterChangesTrait, ParamID, ParamValue, String128, TChar,
};
use vst3::Steinberg::{
    IBStream, IBStreamTrait, TUID, int32, int64, kInvalidArgument, kNotImplemented, kResultFalse,
    kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

/// Called with `(param_id, normalized_value)` when the plugin's own editor changes
/// a parameter.
pub type EditCallback = Box<dyn Fn(u32, f32) + Send + Sync>;

pub(crate) fn tchar_to_string(src: &[TChar]) -> String {
    let len = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    String::from_utf16_lossy(&src[..len])
}

pub(crate) fn char8_to_string(src: &[c_char]) -> String {
    let len = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    String::from_utf8_lossy(&src[..len].iter().map(|&c| c as u8).collect::<Vec<_>>()).into_owned()
}

/// The `FUnknown* context` passed to every component/controller's `initialize`.
pub(crate) struct HostApplication;

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        let name = unsafe { &mut *name };
        let len = name.len() - 1;
        for (dst, src) in name.iter_mut().zip("live-keys".encode_utf16().take(len)) {
            *dst = src as TChar;
        }
        name["live-keys".len()] = 0;
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        // Only needed for `IMessage`/`IAttributeList`, which plugins with a separate
        // controller use to talk to their processor over `IConnectionPoint`. Every
        // VST3 plugin tested so far is JUCE-based (single component), which doesn't.
        unsafe { *obj = std::ptr::null_mut() };
        kNotImplemented
    }
}

/// Receives parameter edits from the plugin's own editor.
pub(crate) struct ComponentHandler {
    pub on_edit: Mutex<Option<EditCallback>>,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        if let Some(on_edit) = &*self.on_edit.lock().unwrap() {
            on_edit(id, value as f32);
        }
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        // Latency/IO/param-title changes would need a re-setup or a refreshed
        // parameter list; neither is supported yet, so the plugin keeps running
        // with what it reported at load time.
        log::info!("VST3 plugin requested restartComponent (flags {flags:#x}), ignored");
        kResultOk
    }
}

/// A fixed-capacity event list, filled by the host before `process` (input) or by
/// the plugin during it (output). Never reallocates after construction.
pub(crate) struct EventList {
    events: UnsafeCell<Vec<Event>>,
}

// SAFETY: an event list is only ever touched by the thread currently running the
// plugin's `process` — the host fills it right before the call, the plugin reads or
// appends during it, and the host clears it right after.
unsafe impl Send for EventList {}
unsafe impl Sync for EventList {}

impl EventList {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: UnsafeCell::new(Vec::with_capacity(capacity)),
        }
    }

    pub fn clear(&self) {
        unsafe { (*self.events.get()).clear() };
    }

    /// The events currently in the list. Only call between `process` calls (or
    /// from inside one), like everything else here.
    pub fn events(&self) -> &[Event] {
        unsafe { &*self.events.get() }
    }

    /// Appends `event` unless the list is full, in which case it's dropped rather
    /// than reallocating on the audio thread.
    pub fn push(&self, event: Event) -> bool {
        let events = unsafe { &mut *self.events.get() };
        if events.len() < events.capacity() {
            events.push(event);
            true
        } else {
            false
        }
    }
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        unsafe { (&*self.events.get()).len() as int32 }
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        match unsafe { (&*self.events.get()).get(index as usize) } {
            Some(event) => {
                unsafe { *e = *event };
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if self.push(unsafe { *e }) { kResultOk } else { kResultFalse }
    }
}

/// One parameter's changes within a block. Every change this host sends lands at
/// sample offset 0 (there's no sample-accurate source to place it elsewhere), so a
/// single point is enough: a later change in the same block just overwrites it.
pub(crate) struct ParamValueQueue {
    id: Cell<ParamID>,
    value: Cell<ParamValue>,
}

// SAFETY: as for `EventList` — only touched by the thread running `process`.
unsafe impl Send for ParamValueQueue {}
unsafe impl Sync for ParamValueQueue {}

impl Class for ParamValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        1
    }

    unsafe fn getPoint(
        &self,
        index: int32,
        sample_offset: *mut int32,
        value: *mut ParamValue,
    ) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        unsafe {
            *sample_offset = 0;
            *value = self.value.get();
        }
        kResultOk
    }

    unsafe fn addPoint(&self, _sample_offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        self.value.set(value);
        if !index.is_null() {
            unsafe { *index = 0 };
        }
        kResultOk
    }
}

/// A fixed pool of per-parameter queues for one block's parameter changes.
pub(crate) struct ParameterChanges {
    queues: Vec<ComWrapper<ParamValueQueue>>,
    used: Cell<usize>,
}

// SAFETY: as for `EventList` — only touched by the thread running `process`.
unsafe impl Send for ParameterChanges {}
unsafe impl Sync for ParameterChanges {}

impl ParameterChanges {
    pub fn new(capacity: usize) -> Self {
        Self {
            queues: (0..capacity)
                .map(|_| {
                    ComWrapper::new(ParamValueQueue {
                        id: Cell::new(0),
                        value: Cell::new(0.0),
                    })
                })
                .collect(),
            used: Cell::new(0),
        }
    }

    pub fn clear(&self) {
        self.used.set(0);
    }

    /// Records `value` for `id` in this block, returning the index of its queue.
    /// `None` if every queue is taken by other parameters already.
    pub fn set(&self, id: ParamID, value: ParamValue) -> Option<usize> {
        let used = self.used.get();
        let index = match self.queues[..used].iter().position(|queue| queue.id.get() == id) {
            Some(index) => index,
            None if used < self.queues.len() => {
                self.queues[used].id.set(id);
                self.used.set(used + 1);
                used
            }
            None => return None,
        };
        self.queues[index].value.set(value);
        Some(index)
    }

    fn queue_ptr(&self, index: usize) -> *mut IParamValueQueue {
        self.queues[index]
            .as_com_ref::<IParamValueQueue>()
            .unwrap()
            .as_ptr()
    }
}

impl Class for ParameterChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParameterChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if (0..self.used.get() as int32).contains(&index) {
            self.queue_ptr(index as usize)
        } else {
            std::ptr::null_mut()
        }
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        let id = unsafe { *id };
        let used = self.used.get();
        let slot = match self.queues[..used].iter().position(|queue| queue.id.get() == id) {
            Some(slot) => slot,
            None => match self.set(id, 0.0) {
                Some(slot) => slot,
                None => return std::ptr::null_mut(),
            },
        };
        if !index.is_null() {
            unsafe { *index = slot as int32 };
        }
        self.queue_ptr(slot)
    }
}

/// A growable in-memory `IBStream`, for handing a component's state to its
/// controller (`getState` → `setComponentState`). Main thread only.
pub(crate) struct MemoryStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl MemoryStream {
    pub fn new() -> Self {
        Self {
            data: RefCell::new(Vec::new()),
            pos: Cell::new(0),
        }
    }

    pub fn rewind(&self) {
        self.pos.set(0);
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_bytes_read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let count = (num_bytes.max(0) as usize).min(data.len() - pos);
        unsafe { std::ptr::copy_nonoverlapping(data[pos..].as_ptr(), buffer as *mut u8, count) };
        self.pos.set(pos + count);
        if !num_bytes_read.is_null() {
            unsafe { *num_bytes_read = count as int32 };
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_bytes_written: *mut int32) -> tresult {
        let count = num_bytes.max(0) as usize;
        let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, count) };
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + count {
            data.resize(pos + count, 0);
        }
        data[pos..pos + count].copy_from_slice(bytes);
        self.pos.set(pos + count);
        if !num_bytes_written.is_null() {
            unsafe { *num_bytes_written = count as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        // IBStream::IStreamSeekMode: 0 = from start, 1 = from current, 2 = from end.
        let base = match mode {
            0 => 0,
            1 => self.pos.get() as int64,
            2 => self.data.borrow().len() as int64,
            _ => return kInvalidArgument,
        };
        let new_pos = (base + pos).max(0);
        self.pos.set(new_pos as usize);
        if !result.is_null() {
            unsafe { *result = new_pos };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if !pos.is_null() {
            unsafe { *pos = self.pos.get() as int64 };
        }
        kResultOk
    }
}
