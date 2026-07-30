use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use live_keys_core::{HostedPlugin, MidiEvent, ParamChange, ParamProducer, midi_ring_buffer, param_ring_buffer};
use live_keys_lv2_host::{ControlPortInfo, Lv2Host, PluginInfo, livi, lv2_raw};

use std::path::PathBuf;
use std::sync::Arc;

use crate::audio::find_output_device;
use crate::midi::find_midi_port;
use crate::paths::build_lv2_path;

const MIDI_RING_CAPACITY: usize = 1024;
const PARAM_RING_CAPACITY: usize = 256;

/// Everything a plugin's own GUI needs, captured at load time since the running
/// `Lv2Plugin` itself gets moved into the audio callback and isn't reachable from
/// `Engine` afterwards.
pub struct LoadedPluginMeta {
    pub plugin: livi::Plugin,
    pub features: Arc<livi::Features>,
    pub raw_handle: lv2_raw::core::LV2Handle,
    /// Control-input port metadata, for a parameter-editor UI (macro knobs, a
    /// fallback UI for GUI-less plugins, future MIDI binding). Captured at load
    /// time alongside the rest of this struct since the running `Lv2Plugin`
    /// itself isn't reachable once it's moved into the audio callback.
    pub control_ports: Vec<ControlPortInfo>,
}

/// Owns the LV2 plugin world and the currently running audio/MIDI stream, if any.
/// Real-time wiring (ring buffers, no allocation in the audio callback) lives here so
/// every host (CLI, GUI) shares one implementation instead of drifting apart.
pub struct Engine {
    audio_host: cpal::Host,
    lv2: Lv2Host,
    max_block_size: usize,
    stream: Option<cpal::Stream>,
    midi_connection: Option<midir::MidiInputConnection<()>>,
    loaded_plugin_name: Option<String>,
    loaded_plugin_meta: Option<LoadedPluginMeta>,
    param_tx: Option<ParamProducer>,
}

impl Engine {
    /// `max_block_size` must be at least as large as the largest number of frames the
    /// audio backend will ever hand us in one callback.
    pub fn new(max_block_size: usize) -> Self {
        Self {
            audio_host: cpal::default_host(),
            lv2: Lv2Host::new(max_block_size),
            max_block_size,
            stream: None,
            midi_connection: None,
            loaded_plugin_name: None,
            loaded_plugin_meta: None,
            param_tx: None,
        }
    }

    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.lv2.list_plugins()
    }

    pub fn loaded_plugin_meta(&self) -> Option<&LoadedPluginMeta> {
        self.loaded_plugin_meta.as_ref()
    }

    /// Control-input ports of the currently loaded plugin, or an empty slice if
    /// nothing is loaded or it has none.
    pub fn control_ports(&self) -> &[ControlPortInfo] {
        self.loaded_plugin_meta
            .as_ref()
            .map_or(&[], |meta| meta.control_ports.as_slice())
    }

    /// Rebuilds the plugin world so it also searches `extra_dirs`, in addition to the
    /// default LV2 search paths (see [`crate::default_plugin_search_paths`]). Stops any
    /// currently running plugin first — rescanning would otherwise leave it depending
    /// on a `World` that's about to be dropped.
    pub fn rescan_plugins(&mut self, extra_dirs: &[PathBuf]) {
        self.stop();
        // SAFETY: only ever called from the control thread; nothing else in this
        // process reads or writes environment variables concurrently.
        unsafe { std::env::set_var("LV2_PATH", build_lv2_path(extra_dirs)) };
        self.lv2 = Lv2Host::new(self.max_block_size);
    }

    pub fn loaded_plugin_name(&self) -> Option<&str> {
        self.loaded_plugin_name.as_deref()
    }

    /// Stops the running stream and MIDI connection, if any.
    pub fn stop(&mut self) {
        self.drop_stream_async();
        self.loaded_plugin_name = None;
        self.loaded_plugin_meta = None;
        self.param_tx = None;
    }

    /// Moves any currently running stream/MIDI connection to a throwaway thread for
    /// teardown, rather than dropping them in place. `cpal`'s ALSA `Stream::drop()`
    /// joins its own internal audio thread; if that thread is stuck for any reason
    /// (observed after a previous session's plugin UI wedged internally — see
    /// `live-keys-lv2-ui`'s docs), dropping in place would block the caller —
    /// typically the GUI thread, e.g. when loading a new plugin — waiting on it
    /// forever. A stuck old thread is abandoned (leaked until the app exits) rather
    /// than allowed to freeze anything new.
    fn drop_stream_async(&mut self) {
        let stream = self.stream.take();
        let midi_connection = self.midi_connection.take();
        if stream.is_some() || midi_connection.is_some() {
            std::thread::spawn(move || {
                drop(stream);
                drop(midi_connection);
            });
        }
    }

    /// Requests that a control-port input on the currently loaded plugin be set to
    /// `value`, applied on the next audio block. Intended for a plugin's own GUI to
    /// call from a non-audio thread. Dropped (and logged) if nothing is loaded or if
    /// the queue is full — the latter means the audio thread isn't draining it,
    /// which is worth knowing about even though there's little to do but drop.
    pub fn send_param_change(&mut self, port_index: u32, value: f32) {
        if let Some(param_tx) = &mut self.param_tx
            && let Err(err) = param_tx.push(ParamChange { port_index, value })
        {
            log::warn!("dropped param change, queue full: {err}");
        }
    }

    /// Instantiates the LV2 plugin at `plugin_uri` and starts streaming it to
    /// `audio_filter` (or the default output device) driven by `midi_filter` (or the
    /// first MIDI input port). Replaces any previously running stream.
    pub fn load_and_start(
        &mut self,
        plugin_uri: &str,
        audio_filter: Option<&str>,
        midi_filter: Option<&str>,
    ) -> Result<()> {
        let output_device = find_output_device(&self.audio_host, audio_filter)?;
        let output_config = output_device.default_output_config()?;
        if output_config.sample_format() != cpal::SampleFormat::F32 {
            anyhow::bail!(
                "output device uses sample format {:?}, only F32 is supported right now",
                output_config.sample_format()
            );
        }
        let sample_rate = f64::from(output_config.sample_rate());
        let channels = output_config.channels() as usize;
        log::info!("Output device: {output_device} ({channels} ch @ {sample_rate} Hz)");

        let mut plugin = self
            .lv2
            .instantiate(plugin_uri, sample_rate)
            .with_context(|| format!("failed to instantiate LV2 plugin '{plugin_uri}'"))?;
        log::info!("Loaded plugin: {}", plugin.name());
        let plugin_name = plugin.name().to_string();
        let plugin_meta = LoadedPluginMeta {
            plugin: plugin.plugin().clone(),
            features: plugin.features().clone(),
            raw_handle: plugin.raw_handle(),
            control_ports: plugin.control_ports(),
        };
        let plugin_channels = plugin.audio_output_count().max(1);

        let (mut midi_tx, mut midi_rx) = midi_ring_buffer(MIDI_RING_CAPACITY);
        let (param_tx, mut param_rx) = param_ring_buffer(PARAM_RING_CAPACITY);

        let midi_input = midir::MidiInput::new("live-keys")?;
        let midi_ports = midi_input.ports();
        let midi_port = find_midi_port(&midi_input, &midi_ports, midi_filter)?;
        log::info!("MIDI input: {}", midi_input.port_name(midi_port)?);
        let midi_connection = midi_input
            .connect(
                midi_port,
                "live-keys-input",
                move |_stamp, bytes, _| {
                    // No sample-accurate timestamp is available here, so events are
                    // applied at the start of whichever audio block consumes them.
                    if let Some(event) = MidiEvent::from_raw(0, bytes)
                        && let Err(err) = midi_tx.push(event)
                    {
                        // This is midir's own callback thread, not the audio thread,
                        // so logging here is safe. A full queue means the audio
                        // thread isn't draining it — dropped notes are worth knowing
                        // about even though there's little to do but drop.
                        log::warn!("dropped MIDI event, queue full: {err}");
                    }
                },
                (),
            )
            .map_err(|err| anyhow::anyhow!("failed to connect to MIDI input: {err}"))?;

        let mut midi_batch: Vec<MidiEvent> = Vec::with_capacity(MIDI_RING_CAPACITY);
        let max_block_size = self.max_block_size;

        let stream_config: cpal::StreamConfig = output_config.into();
        let stream = output_device.build_output_stream(
            stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let frames = (data.len() / channels).min(max_block_size);

                midi_batch.clear();
                while let Ok(event) = midi_rx.pop() {
                    midi_batch.push(event);
                }
                while let Ok(change) = param_rx.pop() {
                    plugin.set_control(change.port_index, change.value);
                }
                plugin.process(&midi_batch, frames);

                for (frame_idx, frame) in data.chunks_mut(channels).enumerate().take(frames) {
                    for (ch, sample) in frame.iter_mut().enumerate() {
                        *sample = plugin.audio_output(ch % plugin_channels)[frame_idx];
                    }
                }
            },
            |err| log::error!("audio stream error: {err}"),
            None,
        )?;
        stream.play()?;

        // Tear down any previously running stream/connection asynchronously — see
        // `drop_stream_async`'s docs for why this must never block here.
        self.drop_stream_async();
        self.stream = Some(stream);
        self.midi_connection = Some(midi_connection);
        self.loaded_plugin_name = Some(plugin_name);
        self.loaded_plugin_meta = Some(plugin_meta);
        self.param_tx = Some(param_tx);
        Ok(())
    }
}
