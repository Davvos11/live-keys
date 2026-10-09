use anyhow::Result;
use clap::Parser;
use live_keys_engine::{Engine, list_logical_output_devices, list_midi_ports};

/// Upper bound on frames-per-callback we'll ever be asked to render. Real device
/// buffer sizes are almost always far smaller than this.
const MAX_BLOCK_SIZE: usize = 8192;
/// Helm: a full-featured polyphonic synth, good for a first "does sound come out" test.
const DEFAULT_PLUGIN_ID: &str = "http://tytel.org/helm";

/// A live-performance host for LV2 and VST3 instrument plugins, driven by a MIDI
/// keyboard.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// List all MIDI input ports and exit.
    #[arg(long)]
    list_midi: bool,

    /// List logical audio output devices and exit.
    #[arg(long)]
    list_audio: bool,

    /// List discovered instrument plugins (id, format, name) and exit.
    #[arg(long)]
    list: bool,

    /// Id of the instrument plugin to load, as printed by `--list`: an LV2 plugin's
    /// URI, or `vst3:` + a VST3 class ID.
    #[arg(long, alias = "plugin-uri", default_value = DEFAULT_PLUGIN_ID)]
    plugin: String,

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
        for port in list_midi_ports()? {
            println!("{port}");
        }
        return Ok(());
    }

    if cli.list_audio {
        for device in list_logical_output_devices()? {
            println!("{device}");
        }
        return Ok(());
    }

    let mut engine = Engine::new(MAX_BLOCK_SIZE);

    if cli.list {
        for plugin in engine.list_plugins() {
            if plugin.is_instrument {
                println!("{}\t{}\t{}", plugin.id, plugin.format.label(), plugin.name);
            }
        }
        return Ok(());
    }

    engine.start(cli.audio_device.as_deref(), cli.midi_port.as_deref())?;
    engine.load_into_slot(0, &cli.plugin)?;

    log::info!("Streaming. Press Ctrl+C to exit.");
    std::thread::park();
    Ok(())
}
