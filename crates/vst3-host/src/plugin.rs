//! Instantiating a VST3 plugin, split into the half the audio thread owns
//! ([`Vst3Plugin`]) and the half the main thread owns ([`Vst3Controller`]).

use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use live_keys_core::{HostedPlugin, MidiEvent, ParamInfo};
use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kPitchBend};
use vst3::Steinberg::Vst::Event_::EventFlags_::kIsLive;
use vst3::Steinberg::Vst::Event_::EventTypes_::{kNoteOffEvent, kNoteOnEvent, kPolyPressureEvent};
use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_::{kIsHidden, kIsReadOnly};
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_::{
    kContTimeValid, kTempoValid, kTimeSigValid,
};
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, BusInfo, Event, Event__type0, IAudioProcessor,
    IAudioProcessorTrait, IComponent, IComponentHandler, IComponentTrait, IConnectionPoint,
    IConnectionPointTrait, IEditController, IEditControllerTrait, IEventList, IMidiMapping,
    IMidiMappingTrait, IParameterChanges, NoteOffEvent, NoteOnEvent, ParamID, ParameterInfo,
    PolyPressureEvent, ProcessContext, ProcessData, ProcessSetup,
};
use vst3::Steinberg::{
    FUnknown, IBStream, IPluginBaseTrait, IPluginFactoryTrait, TUID, kResultOk, kResultTrue,
};
use vst3::{ComPtr, ComWrapper, Interface};

use crate::com::{
    ComponentHandler, EditCallback, EventList, HostApplication, MemoryStream, ParameterChanges,
    tchar_to_string,
};
use crate::module::Module;

// The bindings type these enums as `u32`, while the methods taking them want `i32`.
const AUDIO: i32 = vst3::Steinberg::Vst::MediaTypes_::kAudio as i32;
const EVENT: i32 = vst3::Steinberg::Vst::MediaTypes_::kEvent as i32;
const INPUT: i32 = vst3::Steinberg::Vst::BusDirections_::kInput as i32;
const OUTPUT: i32 = vst3::Steinberg::Vst::BusDirections_::kOutput as i32;
const REALTIME: i32 = vst3::Steinberg::Vst::ProcessModes_::kRealtime as i32;
const SAMPLE_32: i32 = vst3::Steinberg::Vst::SymbolicSampleSizes_::kSample32 as i32;

/// Most events one block can carry; extra ones in a dense burst are dropped.
const EVENT_CAPACITY: usize = 512;
/// Most distinct parameters one block can change.
const PARAM_CHANGE_CAPACITY: usize = 128;
/// MIDI controllers a VST3 plugin can map to parameters: CC 0-127, plus
/// `kAfterTouch` (128, channel pressure) and `kPitchBend` (129).
const MIDI_CONTROLLERS: usize = 130;
const UNMAPPED: ParamID = ParamID::MAX;

/// What a plugin maps each MIDI channel's controllers to: VST3 has no MIDI CC or
/// pitch-bend events, so a plugin that wants them exposes parameters for them
/// through `IMidiMapping`, and the host translates.
type MidiMap = [[ParamID; MIDI_CONTROLLERS]; 16];

/// The plugin objects both halves share, torn down once neither half is left.
struct Instance {
    component: ComPtr<IComponent>,
    /// `Some` only for a plugin whose controller is a separate object from its
    /// component (needs its own `initialize`/`terminate`).
    separate_controller: Option<ComPtr<IEditController>>,
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    _host: ComWrapper<HostApplication>,
    _module: Arc<Module>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if let Some((component, controller)) = &self.connection {
                component.disconnect(controller.as_ptr());
                controller.disconnect(component.as_ptr());
            }
            if let Some(controller) = &self.separate_controller {
                controller.terminate();
            }
            self.component.terminate();
        }
    }
}

/// Instantiates class `cid` from `module` and gets it ready to process: both halves
/// initialized and connected, controller synced to the component's state, buses
/// activated, processing set up at `sample_rate`/`max_block_size`.
pub(crate) fn instantiate(
    module: Arc<Module>,
    cid: &TUID,
    name: String,
    sample_rate: f64,
    max_block_size: usize,
) -> Result<(Vst3Plugin, Vst3Controller)> {
    let host = ComWrapper::new(HostApplication);
    let host_ptr = host.to_com_ptr::<FUnknown>().unwrap();

    let component: ComPtr<IComponent> = unsafe {
        create_instance(&module, cid).context("factory couldn't create the component")?
    };
    anyhow::ensure!(
        unsafe { component.initialize(host_ptr.as_ptr()) } == kResultOk,
        "component failed to initialize"
    );

    // From here on, dropping `instance` tears down whatever's been set up.
    let mut instance = Instance {
        component: component.clone(),
        separate_controller: None,
        connection: None,
        _host: host,
        _module: module.clone(),
    };

    let controller = match component.cast::<IEditController>() {
        Some(controller) => controller,
        None => {
            let mut controller_cid: TUID = [0; 16];
            anyhow::ensure!(
                unsafe { component.getControllerClassId(&mut controller_cid) } == kResultOk,
                "plugin has no edit controller"
            );
            let controller: ComPtr<IEditController> = unsafe {
                create_instance(&module, &controller_cid)
                    .context("factory couldn't create the edit controller")?
            };
            anyhow::ensure!(
                unsafe { controller.initialize(host_ptr.as_ptr()) } == kResultOk,
                "edit controller failed to initialize"
            );
            instance.separate_controller = Some(controller.clone());
            if let (Some(a), Some(b)) = (
                component.cast::<IConnectionPoint>(),
                controller.cast::<IConnectionPoint>(),
            ) {
                unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
                instance.connection = Some((a, b));
            }
            controller
        }
    };

    // Let the controller start from the component's actual (default) state.
    let stream = ComWrapper::new(MemoryStream::new());
    let stream_ptr = stream.to_com_ptr::<IBStream>().unwrap();
    if unsafe { component.getState(stream_ptr.as_ptr()) } == kResultOk {
        stream.rewind();
        unsafe { controller.setComponentState(stream_ptr.as_ptr()) };
    }

    let handler = ComWrapper::new(ComponentHandler {
        on_edit: Mutex::new(None),
    });
    unsafe {
        controller.setComponentHandler(handler.to_com_ptr::<IComponentHandler>().unwrap().as_ptr())
    };

    let processor = component
        .cast::<IAudioProcessor>()
        .context("component isn't an audio processor")?;

    let output_channels = bus_channel_counts(&component, OUTPUT);
    let input_channels = bus_channel_counts(&component, INPUT);
    log::debug!(
        "{name}: audio outputs {output_channels:?}, audio inputs {input_channels:?}, event inputs {}, separate controller: {}",
        unsafe { component.getBusCount(EVENT, INPUT) },
        instance.separate_controller.is_some()
    );
    // Only the main buses are used; any others (e.g. Surge XT's per-scene outputs)
    // stay inactive but still get buffers, which some plugins assume.
    unsafe {
        if !output_channels.is_empty() {
            component.activateBus(AUDIO, OUTPUT, 0, 1);
        }
        if !input_channels.is_empty() {
            component.activateBus(AUDIO, INPUT, 0, 1);
        }
        if component.getBusCount(EVENT, INPUT) > 0 {
            component.activateBus(EVENT, INPUT, 0, 1);
        }
    }

    unsafe {
        anyhow::ensure!(
            processor.canProcessSampleSize(SAMPLE_32) == kResultTrue,
            "plugin can't process 32-bit float audio"
        );
        let mut setup = ProcessSetup {
            processMode: REALTIME,
            symbolicSampleSize: SAMPLE_32,
            maxSamplesPerBlock: max_block_size as i32,
            sampleRate: sample_rate,
        };
        anyhow::ensure!(
            processor.setupProcessing(&mut setup) == kResultOk,
            "plugin rejected the processing setup ({sample_rate} Hz, {max_block_size} frames)"
        );
        anyhow::ensure!(component.setActive(1) == kResultOk, "plugin failed to activate");
        // Allowed to return kNotImplemented; plugins that don't need it don't care.
        processor.setProcessing(1);
    }

    let instance = Arc::new(instance);
    let midi_map = midi_map(&controller);
    let params = read_params(&controller, &midi_map);
    let plugin = Vst3Plugin::new(
        name,
        instance.clone(),
        processor,
        midi_map,
        &output_channels,
        &input_channels,
        sample_rate,
        max_block_size,
    );
    let controller = Vst3Controller {
        params,
        controller,
        handler,
        _instance: instance,
    };
    Ok((plugin, controller))
}

unsafe fn create_instance<I: Interface>(module: &Module, cid: &TUID) -> Option<ComPtr<I>> {
    let mut obj = std::ptr::null_mut();
    let result = unsafe {
        module
            .factory
            .createInstance(cid.as_ptr(), I::IID.as_ptr() as *const c_char, &mut obj)
    };
    if result != kResultOk {
        return None;
    }
    unsafe { ComPtr::from_raw(obj as *mut I) }
}

fn bus_channel_counts(component: &ComPtr<IComponent>, direction: i32) -> Vec<usize> {
    let count = unsafe { component.getBusCount(AUDIO, direction) };
    (0..count)
        .map(|index| {
            let mut info: BusInfo = unsafe { std::mem::zeroed() };
            if unsafe { component.getBusInfo(AUDIO, direction, index, &mut info) } == kResultOk {
                info.channelCount.max(0) as usize
            } else {
                0
            }
        })
        .collect()
}

fn midi_map(controller: &ComPtr<IEditController>) -> Box<MidiMap> {
    let mut map = Box::new([[UNMAPPED; MIDI_CONTROLLERS]; 16]);
    if let Some(mapping) = controller.cast::<IMidiMapping>() {
        for (channel, controllers) in map.iter_mut().enumerate() {
            for (number, param) in controllers.iter_mut().enumerate() {
                let mut id = 0;
                let result = unsafe {
                    mapping.getMidiControllerAssignment(0, channel as i16, number as i16, &mut id)
                };
                if result == kResultTrue || result == kResultOk {
                    *param = id;
                }
            }
        }
    }
    map
}

/// The parameters worth showing in a parameter editor: not hidden, not read-only,
/// and not a MIDI-controller stand-in from `midi_map` — JUCE plugins expose one per
/// channel and controller (2080 of them), without flagging them hidden.
fn read_params(controller: &ComPtr<IEditController>, midi_map: &MidiMap) -> Vec<ParamInfo> {
    let midi_mapped: std::collections::HashSet<ParamID> =
        midi_map.iter().flatten().copied().filter(|&id| id != UNMAPPED).collect();
    let count = unsafe { controller.getParameterCount() };
    (0..count)
        .filter_map(|index| {
            let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
            if unsafe { controller.getParameterInfo(index, &mut info) } != kResultOk {
                return None;
            }
            if info.flags & (kIsHidden | kIsReadOnly) != 0 || midi_mapped.contains(&info.id) {
                return None;
            }
            Some(ParamInfo {
                id: info.id,
                name: tchar_to_string(&info.title),
                default_value: info.defaultNormalizedValue as f32,
                min_value: 0.0,
                max_value: 1.0,
            })
        })
        .collect()
}

/// The audio-thread half of a running VST3 plugin. All buffers, event lists and
/// parameter queues are allocated at load time.
pub struct Vst3Plugin {
    name: String,
    processor: ComPtr<IAudioProcessor>,
    /// Per output bus, per channel. Only bus 0's are read back.
    output_buffers: Vec<Vec<Vec<f32>>>,
    /// Per bus: pointers into `output_buffers` / at `silence`, which
    /// `output_buses`/`input_buses` point at in turn.
    _output_channel_ptrs: Vec<Vec<*mut f32>>,
    _input_channel_ptrs: Vec<Vec<*mut f32>>,
    output_buses: Vec<AudioBusBuffers>,
    input_buses: Vec<AudioBusBuffers>,
    /// Fed to every input channel (no audio-in routing yet — see the LV2 adapter).
    /// Re-zeroed every block in case a plugin writes to its inputs.
    silence: Vec<f32>,
    events: ComWrapper<EventList>,
    output_events: ComWrapper<EventList>,
    param_changes: ComWrapper<ParameterChanges>,
    output_param_changes: ComWrapper<ParameterChanges>,
    events_ptr: *mut IEventList,
    output_events_ptr: *mut IEventList,
    param_changes_ptr: *mut IParameterChanges,
    output_param_changes_ptr: *mut IParameterChanges,
    midi_map: Box<MidiMap>,
    context: ProcessContext,
    frames_rendered: usize,
    process_failed: bool,
    /// Dropped last, after processing has been switched off in `drop`.
    _instance: Arc<Instance>,
}

// SAFETY: the raw pointers all point into this struct's own heap allocations or at
// COM objects it owns, and every method that touches them takes `&mut self` or is
// only called by whichever single thread currently owns the plugin.
unsafe impl Send for Vst3Plugin {}

impl Vst3Plugin {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        instance: Arc<Instance>,
        processor: ComPtr<IAudioProcessor>,
        midi_map: Box<MidiMap>,
        output_channels: &[usize],
        input_channels: &[usize],
        sample_rate: f64,
        max_block_size: usize,
    ) -> Self {
        let mut output_buffers: Vec<Vec<Vec<f32>>> = output_channels
            .iter()
            .map(|&channels| vec![vec![0.0; max_block_size]; channels])
            .collect();
        let mut silence = vec![0.0; max_block_size];

        let output_channel_ptrs: Vec<Vec<*mut f32>> = output_buffers
            .iter_mut()
            .map(|bus| bus.iter_mut().map(|channel| channel.as_mut_ptr()).collect())
            .collect();
        let silence_ptr = silence.as_mut_ptr();
        let input_channel_ptrs: Vec<Vec<*mut f32>> = input_channels
            .iter()
            .map(|&channels| vec![silence_ptr; channels])
            .collect();
        let bus_buffers = |ptrs: &Vec<Vec<*mut f32>>| -> Vec<AudioBusBuffers> {
            ptrs.iter()
                .map(|channels| AudioBusBuffers {
                    numChannels: channels.len() as i32,
                    silenceFlags: 0,
                    __field0: AudioBusBuffers__type0 {
                        channelBuffers32: channels.as_ptr() as *mut *mut f32,
                    },
                })
                .collect()
        };
        let output_buses = bus_buffers(&output_channel_ptrs);
        let input_buses = bus_buffers(&input_channel_ptrs);

        let events = ComWrapper::new(EventList::new(EVENT_CAPACITY));
        let output_events = ComWrapper::new(EventList::new(EVENT_CAPACITY));
        let param_changes = ComWrapper::new(ParameterChanges::new(PARAM_CHANGE_CAPACITY));
        let output_param_changes = ComWrapper::new(ParameterChanges::new(PARAM_CHANGE_CAPACITY));

        let mut context: ProcessContext = unsafe { std::mem::zeroed() };
        context.state = kContTimeValid | kTempoValid | kTimeSigValid;
        context.sampleRate = sample_rate;
        context.tempo = 120.0;
        context.timeSigNumerator = 4;
        context.timeSigDenominator = 4;

        Self {
            name,
            processor,
            output_buffers,
            _output_channel_ptrs: output_channel_ptrs,
            _input_channel_ptrs: input_channel_ptrs,
            output_buses,
            input_buses,
            silence,
            events_ptr: events.as_com_ref::<IEventList>().unwrap().as_ptr(),
            output_events_ptr: output_events.as_com_ref::<IEventList>().unwrap().as_ptr(),
            param_changes_ptr: param_changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
            output_param_changes_ptr: output_param_changes
                .as_com_ref::<IParameterChanges>()
                .unwrap()
                .as_ptr(),
            events,
            output_events,
            param_changes,
            output_param_changes,
            midi_map,
            context,
            frames_rendered: 0,
            process_failed: false,
            _instance: instance,
        }
    }

    fn push_midi(&mut self, event: &MidiEvent) {
        let bytes = event.as_slice();
        let status = bytes[0];
        if status >= 0xF0 || bytes.len() < 2 {
            return;
        }
        let channel = status & 0x0F;
        let data1 = bytes[1] & 0x7F;
        let data2 = bytes.get(2).copied().unwrap_or(0) & 0x7F;
        let note_event = |r#type: u32, body: Event__type0| Event {
            busIndex: 0,
            sampleOffset: event.frame_offset as i32,
            ppqPosition: 0.0,
            flags: kIsLive as u16,
            r#type: r#type as u16,
            __field0: body,
        };
        let controller = |number: u32| self.midi_map[usize::from(channel)][number as usize];

        match status & 0xF0 {
            0x90 if data2 > 0 => {
                self.events.push(note_event(
                    kNoteOnEvent,
                    Event__type0 {
                        noteOn: NoteOnEvent {
                            channel: channel.into(),
                            pitch: data1.into(),
                            tuning: 0.0,
                            velocity: f32::from(data2) / 127.0,
                            length: 0,
                            noteId: -1,
                        },
                    },
                ));
            }
            0x80 | 0x90 => {
                self.events.push(note_event(
                    kNoteOffEvent,
                    Event__type0 {
                        noteOff: NoteOffEvent {
                            channel: channel.into(),
                            pitch: data1.into(),
                            velocity: f32::from(data2) / 127.0,
                            noteId: -1,
                            tuning: 0.0,
                        },
                    },
                ));
            }
            0xA0 => {
                self.events.push(note_event(
                    kPolyPressureEvent,
                    Event__type0 {
                        polyPressure: PolyPressureEvent {
                            channel: channel.into(),
                            pitch: data1.into(),
                            pressure: f32::from(data2) / 127.0,
                            noteId: -1,
                        },
                    },
                ));
            }
            0xB0 => {
                let id = controller(u32::from(data1));
                self.set_mapped(id, f64::from(data2) / 127.0);
            }
            0xD0 => {
                let id = controller(kAfterTouch);
                self.set_mapped(id, f64::from(data1) / 127.0);
            }
            0xE0 => {
                let id = controller(kPitchBend);
                let bend = u16::from(data1) | (u16::from(data2) << 7);
                self.set_mapped(id, f64::from(bend) / 16383.0);
            }
            _ => {}
        }
    }

    fn set_mapped(&self, id: ParamID, value: f64) {
        if id != UNMAPPED {
            self.param_changes.set(id, value);
        }
    }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        unsafe {
            self.processor.setProcessing(0);
            self._instance.component.setActive(0);
        }
    }
}

impl HostedPlugin for Vst3Plugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn audio_output_count(&self) -> usize {
        self.output_buffers.first().map_or(0, Vec::len)
    }

    fn process(&mut self, midi_in: &[MidiEvent], frames: usize) {
        for event in midi_in {
            self.push_midi(event);
        }
        self.silence[..frames].fill(0.0);

        let mut data = ProcessData {
            processMode: REALTIME,
            symbolicSampleSize: SAMPLE_32,
            numSamples: frames as i32,
            numInputs: self.input_buses.len() as i32,
            numOutputs: self.output_buses.len() as i32,
            inputs: self.input_buses.as_mut_ptr(),
            outputs: self.output_buses.as_mut_ptr(),
            inputParameterChanges: self.param_changes_ptr,
            outputParameterChanges: self.output_param_changes_ptr,
            inputEvents: self.events_ptr,
            outputEvents: self.output_events_ptr,
            processContext: &mut self.context,
        };
        let result = unsafe { self.processor.process(&mut data) };
        if result != kResultOk && !self.process_failed {
            // Logged once, not every block: a rare error path on the audio thread.
            self.process_failed = true;
            log::error!("VST3 plugin '{}' failed to process (result {result})", self.name);
        }

        self.context.continousTimeSamples += frames as i64;
        self.frames_rendered = frames;
        self.events.clear();
        self.output_events.clear();
        self.param_changes.clear();
        self.output_param_changes.clear();
    }

    fn audio_output(&self, channel: usize) -> &[f32] {
        &self.output_buffers[0][channel][..self.frames_rendered]
    }

    fn set_param(&mut self, id: u32, value: f32) {
        self.param_changes.set(id, f64::from(value));
    }
}

/// The main-thread half of a running VST3 plugin: its edit controller. Every call
/// into it must come from the main (GUI) thread, per the VST3 threading rules.
pub struct Vst3Controller {
    controller: ComPtr<IEditController>,
    handler: ComWrapper<ComponentHandler>,
    params: Vec<ParamInfo>,
    _instance: Arc<Instance>,
}

impl Vst3Controller {
    /// The plugin's visible, writable parameters, with normalized `0.0..=1.0`
    /// values.
    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// Tells the controller (and so the plugin's own editor, if open) about a
    /// parameter change the host made. Doesn't reach the audio processor — that
    /// goes through the processor's own parameter queue.
    pub fn set_param_normalized(&self, id: u32, value: f32) {
        unsafe { self.controller.setParamNormalized(id, f64::from(value)) };
    }

    /// Sets what gets called when the plugin's own editor changes a parameter, on
    /// whatever thread the plugin calls from (typically the main thread). The
    /// callback should forward the change to the audio processor.
    pub fn set_edit_callback(&self, callback: Option<EditCallback>) {
        *self.handler.on_edit.lock().unwrap() = callback;
    }

    /// For hosting the plugin's own editor (`createView`).
    pub fn edit_controller(&self) -> &ComPtr<IEditController> {
        &self.controller
    }
}
