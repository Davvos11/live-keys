use anyhow::{Context, Result};
use live_keys_core::{MidiEvent, Producer};
use live_keys_surfaces::Message;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::mapper::MapperInput;

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

/// A MIDI event from a device shown as a control surface (see
/// [`Engine::set_surface_devices`](crate::Engine::set_surface_devices)).
#[derive(Debug, Clone)]
pub struct SurfaceEvent {
    /// The device's [`midi_device_name`].
    pub device: Arc<str>,
    pub event: MidiEvent,
}

/// One device's messages that belong to mapped controls (MIDI learn), so they
/// don't reach the slots. Read by the device's MIDI callback, set from the
/// control thread: atomics.
#[derive(Default)]
pub(crate) struct Claims {
    /// Per channel, a bit per controller.
    cc: [[AtomicU64; 2]; 16],
    /// Per channel, a bit per note (its note-ons, note-offs and poly pressure).
    notes: [[AtomicU64; 2]; 16],
    /// Per channel: bit 0 pitch bend, bit 1 channel pressure.
    other: [AtomicU8; 16],
    /// Every event (a port that only carries a surface's controls).
    all: AtomicBool,
}

impl Claims {
    fn clear(&self) {
        for channel in 0..16 {
            for word in 0..2 {
                self.cc[channel][word].store(0, Ordering::Relaxed);
                self.notes[channel][word].store(0, Ordering::Relaxed);
            }
            self.other[channel].store(0, Ordering::Relaxed);
        }
        self.all.store(false, Ordering::Relaxed);
    }

    fn claim(&self, channel: u8, message: Message) {
        let channel = usize::from(channel & 0x0F);
        let bit = |bits: &[AtomicU64; 2], number: u8| {
            bits[usize::from(number >> 6) & 1].fetch_or(1 << (number & 63), Ordering::Relaxed);
        };
        match message {
            Message::Cc(number) => bit(&self.cc[channel], number),
            Message::Note(number) => bit(&self.notes[channel], number),
            Message::PitchBend => drop(self.other[channel].fetch_or(1, Ordering::Relaxed)),
            Message::ChannelPressure => drop(self.other[channel].fetch_or(2, Ordering::Relaxed)),
        }
    }

    fn claimed(&self, event: &MidiEvent) -> bool {
        if self.all.load(Ordering::Relaxed) {
            return true;
        }
        let bytes = event.as_slice();
        let (Some(&status), number) = (bytes.first(), bytes.get(1).copied().unwrap_or(0)) else { return false };
        let channel = usize::from(status & 0x0F);
        let bit = |bits: &[AtomicU64; 2]| {
            bits[usize::from(number >> 6) & 1].load(Ordering::Relaxed) & (1 << (number & 63)) != 0
        };
        match status & 0xF0 {
            0xB0 => bit(&self.cc[channel]),
            0x80 | 0x90 | 0xA0 => bit(&self.notes[channel]),
            0xE0 => self.other[channel].load(Ordering::Relaxed) & 1 != 0,
            0xD0 => self.other[channel].load(Ordering::Relaxed) & 2 != 0,
            _ => false,
        }
    }
}

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
    /// Whether its events also go to the mapping thread.
    surface: Arc<AtomicBool>,
    _connection: midir::MidiInputConnection<()>,
}

/// Connects MIDI devices to the engine's numbered MIDI inputs. Each input has a
/// set of devices (by [`midi_device_name`]); whichever of them are plugged in
/// are connected, and [`MidiInputs::sync`] connects ones that turn up later and
/// lets go of ones that disappeared. A device on several inputs is connected
/// once, its events tagged with all of them. Independent of the audio stream:
/// without one, events are dropped. Devices shown as control surfaces are
/// connected too (on an input or not), and their events also go to the mapping
/// thread; those it claims (see [`MidiInputs::set_claims`]) don't reach the
/// slots.
pub(crate) struct MidiInputs {
    pub tx: SharedMidiTx,
    /// Per input, its devices.
    devices: Vec<Vec<String>>,
    /// Devices shown as control surfaces.
    surfaces: Vec<String>,
    mapper_tx: mpsc::SyncSender<MapperInput>,
    /// By device name; kept once made, so connections can hold on to theirs.
    claims: HashMap<String, Arc<Claims>>,
    /// By full port name.
    connections: HashMap<String, Connection>,
}

impl MidiInputs {
    pub fn new(mapper_tx: mpsc::SyncSender<MapperInput>) -> Self {
        Self {
            tx: Arc::new(Mutex::new(None)),
            devices: Vec::new(),
            surfaces: Vec::new(),
            mapper_tx,
            claims: HashMap::new(),
            connections: HashMap::new(),
        }
    }

    /// Sets the messages each device's mapped controls claim (by device
    /// name), and the devices whose every event is claimed.
    pub fn set_claims(&mut self, claims: &[(String, u8, Message)], whole: &[String]) {
        for table in self.claims.values() {
            table.clear();
        }
        for (device, channel, message) in claims {
            self.claims_of(device).claim(*channel, *message);
        }
        for device in whole {
            self.claims_of(device).all.store(true, Ordering::Relaxed);
        }
    }

    fn claims_of(&mut self, device: &str) -> Arc<Claims> {
        self.claims.entry(device.to_string()).or_default().clone()
    }

    /// Sets the devices shown as control surfaces and connects accordingly.
    pub fn set_surfaces(&mut self, devices: Vec<String>) -> Vec<String> {
        self.surfaces = devices;
        self.sync()
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
        let wanted: HashMap<&str, (u32, bool)> = ports
            .iter()
            .map(|port| {
                let device = midi_device_name(port);
                (port.as_str(), (self.inputs_of(device), self.surfaces.iter().any(|name| name == device)))
            })
            .filter(|&(_, (inputs, surface))| inputs != 0 || surface)
            .collect();
        self.connections.retain(|port, connection| match wanted.get(port.as_str()) {
            Some(&(inputs, surface)) => {
                connection.inputs.store(inputs, Ordering::Relaxed);
                connection.surface.store(surface, Ordering::Relaxed);
                true
            }
            None => {
                log::info!("MIDI input disconnected: {port}");
                false
            }
        });
        for (&port, &(inputs, surface)) in &wanted {
            if self.connections.contains_key(port) {
                continue;
            }
            let claims = self.claims_of(midi_device_name(port));
            match connect(port, inputs, surface, claims, self.tx.clone(), self.mapper_tx.clone()) {
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

fn connect(
    port_name: &str,
    inputs: u32,
    surface: bool,
    claims: Arc<Claims>,
    tx: SharedMidiTx,
    mapper_tx: mpsc::SyncSender<MapperInput>,
) -> Result<Connection> {
    let midi_input = midir::MidiInput::new("live-keys")?;
    let port = midi_input
        .ports()
        .into_iter()
        .find(|port| midi_input.port_name(port).is_ok_and(|name| name == port_name))
        .context("port went away")?;
    let inputs = Arc::new(AtomicU32::new(inputs));
    let callback_inputs = inputs.clone();
    let surface = Arc::new(AtomicBool::new(surface));
    let callback_surface = surface.clone();
    let device: Arc<str> = midi_device_name(port_name).into();
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
                let surface = callback_surface.load(Ordering::Relaxed);
                if surface {
                    // Full: the mapping thread has stalled; the move is lost.
                    let _ = mapper_tx.try_send(MapperInput::Event(SurfaceEvent { device: device.clone(), event }));
                }
                let inputs = callback_inputs.load(Ordering::Relaxed);
                if inputs != 0
                    && !(surface && claims.claimed(&event))
                    && let Ok(mut tx) = tx.lock()
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
        surface,
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

    #[test]
    fn claims_by_channel_and_number() {
        let claims = Claims::default();
        claims.claim(9, Message::Note(36));
        claims.claim(0, Message::Cc(100));
        let claimed = |bytes: &[u8]| claims.claimed(&MidiEvent::from_raw(0, bytes).unwrap());
        assert!(claimed(&[0x99, 36, 100]) && claimed(&[0x89, 36, 0]) && claimed(&[0xA9, 36, 5]));
        assert!(!claimed(&[0x90, 36, 100]) && !claimed(&[0x99, 37, 100]));
        assert!(claimed(&[0xB0, 100, 1]) && !claimed(&[0xB0, 36, 1]) && !claimed(&[0xE0, 0, 64]));
        claims.clear();
        assert!(!claimed(&[0xB0, 100, 1]));
    }
}
