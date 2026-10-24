use anyhow::{Context, Result};
use live_keys_core::{MidiEvent, Producer};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Most MIDI inputs the engine routes (bits of [`MidiFilter::inputs`](live_keys_core::MidiFilter)).
pub const MAX_MIDI_INPUTS: usize = 8;

/// A MIDI event on its way to the audio thread, with the MIDI inputs (as a
/// [`MidiFilter::inputs`](live_keys_core::MidiFilter) mask) it arrived on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutedMidi {
    pub inputs: u32,
    pub event: MidiEvent,
}

/// The MIDI queue's producer while a stream runs: shared by every device
/// connection's callback and [`Engine::send_midi`](crate::Engine::send_midi)
/// (none of them the audio thread, so a lock is fine).
pub(crate) type SharedMidiTx = Arc<Mutex<Option<Producer<RoutedMidi>>>>;

/// Lists MIDI input port names.
pub fn list_midi_ports() -> Result<Vec<String>> {
    let midi_input = midir::MidiInput::new("live-keys")?;
    midi_input
        .ports()
        .iter()
        .map(|port| midi_input.port_name(port).map_err(anyhow::Error::from))
        .collect()
}

/// A port name without ALSA's trailing `client:port` address ("LPD8:LPD8 MIDI 1
/// 32:0" → "LPD8:LPD8 MIDI 1"): the address can change when a device is plugged
/// in again, so devices are remembered by this.
pub fn midi_device_name(port_name: &str) -> &str {
    let Some((name, address)) = port_name.rsplit_once(' ') else {
        return port_name;
    };
    let is_address = address
        .split_once(':')
        .is_some_and(|(client, port)| is_number(client) && is_number(port));
    if is_address { name } else { port_name }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The full name of the first MIDI input port whose name contains `name_filter`
/// (case-insensitive), or of the first port if `name_filter` is `None`.
pub fn find_midi_port(name_filter: Option<&str>) -> Result<String> {
    let ports = list_midi_ports()?;
    match name_filter {
        None => ports
            .into_iter()
            .next()
            .context("no MIDI input devices found; connect a MIDI keyboard and try again"),
        Some(filter) => {
            let filter = filter.to_lowercase();
            ports
                .into_iter()
                .find(|name| name.to_lowercase().contains(&filter))
                .with_context(|| format!("no MIDI input port matching '{filter}' found"))
        }
    }
}

/// One open device connection.
struct Connection {
    /// The inputs its events go to; changed in place when assignments change.
    inputs: Arc<AtomicU32>,
    _connection: midir::MidiInputConnection<()>,
}

/// Connects MIDI devices to the engine's numbered MIDI inputs. Each input has a
/// set of devices (by [`midi_device_name`]); whichever of them are plugged in
/// are connected, and [`MidiInputs::sync`] connects ones that turn up later and
/// lets go of ones that disappeared. A device on several inputs is connected
/// once, its events tagged with all of them. Independent of the audio stream:
/// without one, events are dropped.
pub(crate) struct MidiInputs {
    pub tx: SharedMidiTx,
    /// Per input, its devices.
    devices: Vec<Vec<String>>,
    /// By full port name.
    connections: HashMap<String, Connection>,
}

impl MidiInputs {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(Mutex::new(None)),
            devices: Vec::new(),
            connections: HashMap::new(),
        }
    }

    /// Sets each input's devices (at most [`MAX_MIDI_INPUTS`] inputs; more are
    /// ignored) and connects accordingly.
    pub fn set_devices(&mut self, devices: Vec<Vec<String>>) -> Vec<String> {
        self.devices = devices;
        self.devices.truncate(MAX_MIDI_INPUTS);
        self.sync()
    }

    /// Brings connections in line with the ports present now; returns their names.
    pub fn sync(&mut self) -> Vec<String> {
        let ports = match list_midi_ports() {
            Ok(ports) => ports,
            Err(err) => {
                log::warn!("listing MIDI ports failed: {err:#}");
                return Vec::new();
            }
        };
        let wanted: HashMap<&str, u32> = ports
            .iter()
            .map(|port| (port.as_str(), self.inputs_of(midi_device_name(port))))
            .filter(|&(_, inputs)| inputs != 0)
            .collect();
        self.connections.retain(|port, connection| match wanted.get(port.as_str()) {
            Some(&inputs) => {
                connection.inputs.store(inputs, Ordering::Relaxed);
                true
            }
            None => {
                log::info!("MIDI input disconnected: {port}");
                false
            }
        });
        for (&port, &inputs) in &wanted {
            if self.connections.contains_key(port) {
                continue;
            }
            match connect(port, inputs, self.tx.clone()) {
                Ok(connection) => {
                    log::info!("MIDI input connected: {port}");
                    self.connections.insert(port.to_string(), connection);
                }
                // Tried again on the next sync.
                Err(err) => log::warn!("connecting MIDI input '{port}' failed: {err:#}"),
            }
        }
        ports
    }

    /// The inputs `device` is assigned to, as a mask.
    fn inputs_of(&self, device: &str) -> u32 {
        self.devices
            .iter()
            .enumerate()
            .filter(|(_, devices)| devices.iter().any(|name| name == device))
            .fold(0, |mask, (input, _)| mask | 1 << input)
    }
}

fn connect(port_name: &str, inputs: u32, tx: SharedMidiTx) -> Result<Connection> {
    let midi_input = midir::MidiInput::new("live-keys")?;
    let port = midi_input
        .ports()
        .into_iter()
        .find(|port| midi_input.port_name(port).is_ok_and(|name| name == port_name))
        .context("port went away")?;
    let inputs = Arc::new(AtomicU32::new(inputs));
    let callback_inputs = inputs.clone();
    let connection = midi_input
        .connect(
            &port,
            "live-keys-input",
            move |_stamp, bytes, _| {
                // No sample-accurate timestamp is available here, so events are
                // applied at the start of whichever audio block consumes them.
                let Some(event) = MidiEvent::from_raw(0, bytes) else {
                    return;
                };
                let inputs = callback_inputs.load(Ordering::Relaxed);
                if let Ok(mut tx) = tx.lock()
                    && let Some(tx) = tx.as_mut()
                    && let Err(err) = tx.push(RoutedMidi { inputs, event })
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
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(Connection {
        inputs,
        _connection: connection,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_name_drops_only_an_alsa_address() {
        assert_eq!(midi_device_name("LPD8:LPD8 MIDI 1 32:0"), "LPD8:LPD8 MIDI 1");
        assert_eq!(midi_device_name("vmpk output:out 128:0"), "vmpk output:out");
        assert_eq!(midi_device_name("Some Keyboard"), "Some Keyboard");
        assert_eq!(midi_device_name("Port 2:x"), "Port 2:x");
        assert_eq!(midi_device_name("Foo :1"), "Foo :1");
    }
}
