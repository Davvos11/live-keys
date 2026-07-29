use anyhow::{Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use live_keys_core::{HostedPlugin, MidiEvent, midi_ring_buffer};
use live_keys_lv2_host::Lv2Host;

/// Upper bound on frames-per-callback we'll ever be asked to render. Real device
/// buffer sizes are almost always far smaller than this.
const MAX_BLOCK_SIZE: usize = 8192;
const MIDI_RING_CAPACITY: usize = 1024;
/// Helm: a full-featured polyphonic synth, good for a first "does sound come out" test.
const DEFAULT_PLUGIN_URI: &str = "http://tytel.org/helm";

/// A live-performance host for LV2 instrument plugins, driven by a MIDI keyboard.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// List all MIDI input ports and exit.
    #[arg(long)]
    list_midi: bool,

    /// List logical audio output devices and exit.
    #[arg(long)]
    list_audio: bool,

    /// List discovered LV2 instrument plugins and exit.
    #[arg(long)]
    list: bool,

    /// URI of the LV2 instrument plugin to load.
    #[arg(long, default_value = DEFAULT_PLUGIN_URI)]
    plugin_uri: String,

    /// Case-insensitive substring match to select a MIDI input port.
    /// Defaults to the first available port.
    #[arg(long)]
    midi_port: Option<String>,

    /// Case-insensitive substring match to select an audio output device.
    /// Defaults to the system default output device.
    #[arg(long)]
    audio_device: Option<String>,
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    if cli.list_midi {
        let midi_input = midir::MidiInput::new("live-keys")?;
        for port in midi_input.ports() {
            println!("{}", midi_input.port_name(&port)?);
        }
        return Ok(());
    }

    let audio_host = cpal::default_host();
    if cli.list_audio {
        for device in audio_host.output_devices()?.filter(is_logical_output_device) {
            println!("{device}");
        }
        return Ok(());
    }

    let lv2 = Lv2Host::new(MAX_BLOCK_SIZE);

    if cli.list {
        for plugin in lv2.list_plugins() {
            if plugin.is_instrument {
                println!("{}\t{}", plugin.uri, plugin.name);
            }
        }
        return Ok(());
    }

    let plugin_uri = cli.plugin_uri.as_str();

    let output_device = find_output_device(&audio_host, cli.audio_device.as_deref())?;
    let output_config = output_device.default_output_config()?;
    if output_config.sample_format() != cpal::SampleFormat::F32 {
        anyhow::bail!(
            "default output device uses sample format {:?}, only F32 is supported right now",
            output_config.sample_format()
        );
    }
    let sample_rate = f64::from(output_config.sample_rate());
    let channels = output_config.channels() as usize;
    log::info!("Output device: {output_device} ({channels} ch @ {sample_rate} Hz)");

    let mut plugin = lv2
        .instantiate(plugin_uri, sample_rate)
        .with_context(|| format!("failed to instantiate LV2 plugin '{plugin_uri}'"))?;
    log::info!("Loaded plugin: {}", plugin.name());
    let plugin_channels = plugin.audio_output_count().max(1);

    let (mut midi_tx, mut midi_rx) = midi_ring_buffer(MIDI_RING_CAPACITY);

    let midi_input = midir::MidiInput::new("live-keys")?;
    let midi_ports = midi_input.ports();
    let midi_port = find_midi_port(&midi_input, &midi_ports, cli.midi_port.as_deref())?;
    log::info!("MIDI input: {}", midi_input.port_name(midi_port)?);
    let _midi_connection = midi_input
        .connect(
            midi_port,
            "live-keys-input",
            move |_stamp, bytes, _| {
                // No sample-accurate timestamp is available here, so events are
                // applied at the start of whichever audio block consumes them.
                if let Some(event) = MidiEvent::from_raw(0, bytes) {
                    let _ = midi_tx.push(event);
                }
            },
            (),
        )
        .map_err(|err| anyhow::anyhow!("failed to connect to MIDI input: {err}"))?;

    let mut midi_batch: Vec<MidiEvent> = Vec::with_capacity(MIDI_RING_CAPACITY);

    let stream_config: cpal::StreamConfig = output_config.into();
    let stream = output_device.build_output_stream(
        stream_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let frames = (data.len() / channels).min(MAX_BLOCK_SIZE);

            midi_batch.clear();
            while let Ok(event) = midi_rx.pop() {
                midi_batch.push(event);
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

    log::info!("Streaming. Press Ctrl+C to exit.");
    std::thread::park();
    Ok(())
}

/// Selects a MIDI input port by case-insensitive substring match on its name, or the
/// first available port if `name_filter` is `None`.
fn find_midi_port<'a>(
    midi_input: &midir::MidiInput,
    ports: &'a [midir::MidiInputPort],
    name_filter: Option<&str>,
) -> Result<&'a midir::MidiInputPort> {
    match name_filter {
        None => ports
            .first()
            .context("no MIDI input devices found; connect a MIDI keyboard and try again"),
        Some(filter) => {
            let filter = filter.to_lowercase();
            ports
                .iter()
                .find(|port| {
                    midi_input
                        .port_name(port)
                        .is_ok_and(|name| name.to_lowercase().contains(&filter))
                })
                .with_context(|| format!("no MIDI input port matching '{filter}' found"))
        }
    }
}

/// Selects an audio output device by case-insensitive substring match on its name, or
/// the system default output device if `name_filter` is `None`. Only considers
/// logical devices (see [`is_logical_output_device`]).
fn find_output_device(host: &cpal::Host, name_filter: Option<&str>) -> Result<cpal::Device> {
    match name_filter {
        None => host
            .default_output_device()
            .context("no default audio output device found"),
        Some(filter) => {
            let filter = filter.to_lowercase();
            host.output_devices()?
                .filter(is_logical_output_device)
                .find(|device| device.to_string().to_lowercase().contains(&filter))
                .with_context(|| format!("no audio output device matching '{filter}' found"))
        }
    }
}

/// Filters out raw hardware sub-devices (ALSA's numeric `hw:`/`plughw:` names) so only
/// named/logical devices remain (e.g. `default`, `pulse`, `sysdefault:CARD=...`). ALSA
/// enumerates both, which otherwise produces many duplicate entries per sound card. On
/// backends where a device's driver name is unavailable or doesn't look like a raw ALSA
/// id, the device is kept.
fn is_logical_output_device(device: &cpal::Device) -> bool {
    let Ok(description) = device.description() else {
        return true;
    };
    match description.driver() {
        Some(driver) => !driver.starts_with("hw:") && !driver.starts_with("plughw:"),
        None => true,
    }
}
