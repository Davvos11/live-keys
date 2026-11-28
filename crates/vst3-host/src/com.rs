//! COM objects implemented on the host side and handed to a VST3 plugin: the host
//! context, the edit-callback sink, the per-block event and parameter-change
//! lists, and an in-memory stream for state transfer.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::{
    Event, IAttributeList, IAttributeListTrait, IComponentHandler, IMessage, IMessageTrait, IComponentHandlerTrait, IEventList, IEventListTrait,
    IHostApplication, IHostApplicationTrait, IParamValueQueue, IParamValueQueueTrait,
    IParameterChanges, IParameterChangesTrait, ParamID, ParamValue, RestartFlags_, String128, TChar,
};
use vst3::Steinberg::{
    FIDString, IBStream, IBStreamTrait, TUID, int32, int64, kInvalidArgument, kNoInterface,
    kResultFalse, kResultOk, tresult, uint32,
};
use vst3::{Class, ComPtr, ComWrapper, Interface};

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
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        // `IMessage`s (with their `IAttributeList`), which a plugin's controller
        // and processor send each other over `IConnectionPoint`: LSP's editor
        // asks its processor for meters, graphs and state this way.
        let is = |id: *mut TUID, interface: &[u8; 16]| {
            !id.is_null() && unsafe { (*id).iter().zip(interface).all(|(&a, &b)| a as u8 == b) }
        };
        let created = if is(cid, &IMessage::IID) && (iid.is_null() || is(iid, &IMessage::IID)) {
            ComWrapper::new(HostMessage::default()).to_com_ptr::<IMessage>().map(|ptr| ptr.into_raw() as *mut c_void)
        } else if is(cid, &IAttributeList::IID) && (iid.is_null() || is(iid, &IAttributeList::IID)) {
            ComWrapper::new(AttributeList::default())
                .to_com_ptr::<IAttributeList>()
                .map(|ptr| ptr.into_raw() as *mut c_void)
        } else {
            None
        };
        unsafe { *obj = created.unwrap_or(std::ptr::null_mut()) };
        if created.is_some() { kResultOk } else { kNoInterface }
    }
}

/// A message between a plugin's controller and its processor (the host
/// makes them; see [`HostApplication::createInstance`]): an id and attributes.
pub(crate) struct HostMessage {
    id: Mutex<Option<CString>>,
    attributes: ComPtr<IAttributeList>,
}

impl Default for HostMessage {
    fn default() -> Self {
        Self {
            id: Mutex::new(None),
            attributes: ComWrapper::new(AttributeList::default())
                .to_com_ptr::<IAttributeList>()
                .expect("an AttributeList is an IAttributeList"),
        }
    }
}

// SAFETY: the attribute list is a Rust object behind a mutex (`AttributeList`),
// reference-counted atomically by `ComWrapper`.
unsafe impl Send for HostMessage {}
unsafe impl Sync for HostMessage {}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        // Stays valid until the id is set again or the message goes, as the SDK's.
        self.id.lock().unwrap().as_ref().map_or(std::ptr::null(), |id| id.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let id = (!id.is_null()).then(|| unsafe { CStr::from_ptr(id) }.to_owned());
        *self.id.lock().unwrap() = id;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Not reference-counted for the caller, as in the SDK's host message.
        self.attributes.as_ptr()
    }
}

/// An attribute's value. Strings are kept as UTF-16 with their terminating 0.
#[derive(Clone, Debug, PartialEq)]
enum Attribute {
    Int(i64),
    Float(f64),
    String(Vec<TChar>),
    Binary(Vec<u8>),
}

/// The attributes of a [`HostMessage`] (or one made on its own), by id.
#[derive(Default)]
pub(crate) struct AttributeList {
    values: Mutex<HashMap<CString, Attribute>>,
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl AttributeList {
    fn set(&self, id: AttrID, value: Attribute) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        let id = unsafe { CStr::from_ptr(id) }.to_owned();
        self.values.lock().unwrap().insert(id, value);
        kResultOk
    }

    /// `read` applied to attribute `id`, or `kResultFalse` if there's none (or
    /// `read` doesn't take its kind).
    fn get(&self, id: AttrID, read: impl FnOnce(&Attribute) -> bool) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        let values = self.values.lock().unwrap();
        match values.get(unsafe { CStr::from_ptr(id) }) {
            Some(value) if read(value) => kResultOk,
            _ => kResultFalse,
        }
    }
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.set(id, Attribute::Int(value))
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        self.get(id, |attribute| match attribute {
            Attribute::Int(int) if !value.is_null() => {
                unsafe { *value = *int };
                true
            }
            _ => false,
        })
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.set(id, Attribute::Float(value))
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        self.get(id, |attribute| match attribute {
            Attribute::Float(float) if !value.is_null() => {
                unsafe { *value = *float };
                true
            }
            _ => false,
        })
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut chars = Vec::new();
        let mut at = string;
        loop {
            let char = unsafe { *at };
            chars.push(char);
            if char == 0 {
                break;
            }
            at = unsafe { at.add(1) };
        }
        self.set(id, Attribute::String(chars))
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        self.get(id, |attribute| match attribute {
            Attribute::String(chars) if !string.is_null() => {
                // Truncated to the buffer, which always ends in a 0.
                let room = size_in_bytes as usize / std::mem::size_of::<TChar>();
                if room == 0 {
                    return false;
                }
                let count = chars.len().min(room);
                unsafe {
                    std::ptr::copy_nonoverlapping(chars.as_ptr(), string, count);
                    *string.add(count - 1) = 0;
                }
                true
            }
            _ => false,
        })
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let bytes = if size_in_bytes == 0 || data.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }.to_vec()
        };
        self.set(id, Attribute::Binary(bytes))
    }

    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        self.get(id, |attribute| match attribute {
            // The bytes stay where they are until the attribute is set again or
            // the list goes (a `Vec`'s buffer doesn't move with the map's entries).
            Attribute::Binary(bytes) => {
                unsafe {
                    *data = bytes.as_ptr() as *const c_void;
                    *size_in_bytes = bytes.len() as uint32;
                }
                true
            }
            _ => false,
        })
    }
}

/// Receives parameter edits from the plugin's own editor, and its requests to
/// re-read its parameter values.
pub(crate) struct ComponentHandler {
    pub on_edit: Mutex<Option<EditCallback>>,
    /// Set by `restartComponent(kParamValuesChanged)` (or `kReloadComponent`):
    /// the host should read every parameter's value again (Surge XT asks after
    /// a state load). Taken by `Vst3Controller::take_values_changed`.
    pub values_changed: AtomicBool,
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
        // A flag only: plugins may call this from any thread, the audio thread
        // included, and the values are read on the main thread.
        let reread = (RestartFlags_::kParamValuesChanged | RestartFlags_::kReloadComponent) as int32;
        if flags & reread != 0 {
            self.values_changed.store(true, Ordering::Release);
        }
        // Latency/IO/param-title changes would need a re-setup or a refreshed
        // parameter list; neither is supported yet, so the plugin keeps running
        // with what it reported at load time.
        if flags & !reread != 0 {
            log::info!("VST3 plugin requested restartComponent (flags {flags:#x}), other than values ignored");
        }
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

    /// A stream to read `data` from.
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self {
            data: RefCell::new(data),
            pos: Cell::new(0),
        }
    }

    pub fn rewind(&self) {
        self.pos.set(0);
    }

    /// Everything written so far.
    pub fn bytes(&self) -> Vec<u8> {
        self.data.borrow().clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComRef;

    /// What a plugin does: ask the host for an object by class and interface.
    fn create<I: Interface>(class: &[u8; 16]) -> (tresult, Option<ComPtr<I>>) {
        let host = HostApplication;
        let mut cid: TUID = class.map(|b| b as c_char);
        let mut iid: TUID = I::IID.map(|b| b as c_char);
        let mut obj = std::ptr::null_mut();
        let result = unsafe { host.createInstance(&mut cid, &mut iid, &mut obj) };
        (result, unsafe { ComPtr::from_raw(obj as *mut I) })
    }

    #[test]
    fn host_makes_messages_with_attributes() {
        let (result, message) = create::<IMessage>(&IMessage::IID);
        assert_eq!(result, kResultOk);
        let message = message.unwrap();
        unsafe {
            message.setMessageID(c"UIActivate".as_ptr());
            assert_eq!(CStr::from_ptr(message.getMessageID()), c"UIActivate");

            let attributes = ComRef::from_raw(message.getAttributes()).unwrap();
            let mut int = 0;
            assert_eq!(attributes.setInt(c"id".as_ptr(), -7), kResultOk);
            assert_eq!(attributes.getInt(c"id".as_ptr(), &mut int), kResultOk);
            assert_eq!(int, -7);
            // Another kind under that id isn't there.
            let mut float = 0.0;
            assert_eq!(attributes.getFloat(c"id".as_ptr(), &mut float), kResultFalse);

            // A string comes back cut to the buffer, always 0-terminated.
            let text: Vec<TChar> = "hello".encode_utf16().chain([0]).map(|c| c as TChar).collect();
            attributes.setString(c"name".as_ptr(), text.as_ptr());
            let mut short = [1 as TChar; 3];
            assert_eq!(attributes.getString(c"name".as_ptr(), short.as_mut_ptr(), 6), kResultOk);
            assert_eq!(tchar_to_string(&short), "he");
            let mut long = [1 as TChar; 16];
            attributes.getString(c"name".as_ptr(), long.as_mut_ptr(), 32);
            assert_eq!(tchar_to_string(&long), "hello");

            let bytes = [1u8, 2, 3];
            attributes.setBinary(c"mesh".as_ptr(), bytes.as_ptr() as *const c_void, 3);
            let (mut data, mut size) = (std::ptr::null(), 0);
            assert_eq!(attributes.getBinary(c"mesh".as_ptr(), &mut data, &mut size), kResultOk);
            assert_eq!(std::slice::from_raw_parts(data as *const u8, size as usize), bytes);
        }
    }

    #[test]
    fn host_refuses_other_classes() {
        let (result, object) = create::<IMessage>(&IBStream::IID);
        assert_eq!(result, kNoInterface);
        assert!(object.is_none());
    }
}
