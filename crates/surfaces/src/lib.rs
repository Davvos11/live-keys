//! Control surfaces: pictures of MIDI controllers (keyboards with knobs, pad
//! controllers…) as banks of faders, knobs, pads and buttons, each knowing the
//! MIDI message it sends. See `docs/design/midi-learn.md`.
//!
//! Shipped surfaces are JSON files compiled in ([`SHIPPED`]); the user's live in
//! `<data dir>/live-keys/surfaces/*.json`, the same format, replacing a shipped
//! one with the same `id` wholesale.
//!
//! **Format** (JSON, unknown fields rejected): `id`, `name`, `devices`
//! (substrings of a MIDI device name, matched case-insensitively), and `banks`.
//! A bank has a `name`, a `kind` (`fader`, `knob`, `pad`, `button`), its
//! position `x`/`y` in the panel (pixels), `rows`/`columns`, `numbering`
//! (`top-first`, the default, or `bottom-first`: which row control 1 is on),
//! a label `prefix` (controls are `prefix` + their number, unless they have a
//! `label`), a default `channel` (1–16), for pads optionally `pressure`
//! (`channel` or `poly`: what they send while held), and its `controls` in
//! number order, each one message: `{ "cc": n }`, `{ "note": n }`,
//! `{ "pitch-bend": true }` or `{ "channel-pressure": true }`, optionally
//! with its own `channel` and `label`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use live_keys_core::MidiEvent;
use serde::Deserialize;

/// The shipped surfaces, by file name.
///
/// Their MIDI messages come from the makers' published defaults where those
/// could be found (the LX88+'s faders and pots); the rest (the LX88+'s pads, the
/// LPD8) are best guesses still to be checked on the hardware.
pub const SHIPPED: &[(&str, &str)] = &[
    ("nektar-impact-lx88plus.json", include_str!("../surfaces/nektar-impact-lx88plus.json")),
    ("akai-lpd8.json", include_str!("../surfaces/akai-lpd8.json")),
];

/// The kinds of control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlKind {
    Fader,
    Knob,
    Pad,
    Button,
}

impl ControlKind {
    /// The panel's cell for one control of this kind (width, height), in
    /// pixels: a bank is `columns` × `rows` of these.
    pub fn cell_size(self) -> (u32, u32) {
        match self {
            ControlKind::Fader => (28, 100),
            ControlKind::Knob => (44, 54),
            ControlKind::Pad => (44, 44),
            ControlKind::Button => (36, 38),
        }
    }
}

/// Which row a bank's control 1 is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Numbering {
    #[default]
    TopFirst,
    BottomFirst,
}

/// What a pad sends while held, besides its note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pressure {
    Channel,
    Poly,
}

/// The message a control sends (its channel apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Message {
    Cc(u8),
    Note(u8),
    PitchBend,
    ChannelPressure,
}

/// What a control just did, as read from one of its messages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    /// A fader's or knob's position, or a pad's pressure, 0..=1.
    Value(f32),
    /// A pad or button pressed, with its velocity (1 for a CC button), 0..=1.
    Press(f32),
    Release,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SurfaceFile {
    id: String,
    name: String,
    #[serde(default)]
    devices: Vec<String>,
    banks: Vec<BankFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BankFile {
    name: String,
    kind: ControlKind,
    #[serde(default)]
    x: u32,
    #[serde(default)]
    y: u32,
    #[serde(default = "one")]
    rows: u32,
    columns: u32,
    #[serde(default)]
    numbering: Numbering,
    #[serde(default)]
    prefix: String,
    #[serde(default = "one_u8")]
    channel: u8,
    pressure: Option<Pressure>,
    controls: Vec<ControlFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct ControlFile {
    cc: Option<u8>,
    note: Option<u8>,
    #[serde(default)]
    pitch_bend: bool,
    #[serde(default)]
    channel_pressure: bool,
    channel: Option<u8>,
    label: Option<String>,
}

fn one() -> u32 {
    1
}

fn one_u8() -> u8 {
    1
}

impl ControlFile {
    fn message(&self) -> Result<Message, String> {
        let messages = [
            self.cc.map(Message::Cc),
            self.note.map(Message::Note),
            self.pitch_bend.then_some(Message::PitchBend),
            self.channel_pressure.then_some(Message::ChannelPressure),
        ];
        let mut set = messages.into_iter().flatten();
        match (set.next(), set.next()) {
            (Some(Message::Cc(n) | Message::Note(n)), None) if n > 127 => Err(format!("{n} is over 127")),
            (Some(message), None) => Ok(message),
            (None, _) => Err("a control needs a message (cc, note, pitch-bend or channel-pressure)".into()),
            (Some(_), Some(_)) => Err("a control has more than one message".into()),
        }
    }
}

/// One control, placed in its surface.
#[derive(Debug, Clone, PartialEq)]
pub struct Control {
    pub kind: ControlKind,
    pub label: String,
    pub message: Message,
    /// 0..=15.
    pub channel: u8,
    /// A pad's pressure messages.
    pub pressure: Option<Pressure>,
    /// Its cell's top-left corner in the surface, in pixels.
    pub x: u32,
    pub y: u32,
    /// Its bank, as an index into [`Surface::banks`].
    pub bank: usize,
}

/// A bank as laid out (for its frame in the editor, later).
#[derive(Debug, Clone, PartialEq)]
pub struct Bank {
    pub name: String,
    pub kind: ControlKind,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A surface, its controls placed and indexed by message.
#[derive(Debug, Clone)]
pub struct Surface {
    pub id: String,
    pub name: String,
    pub devices: Vec<String>,
    pub banks: Vec<Bank>,
    pub controls: Vec<Control>,
    /// Size of the whole drawing, in pixels.
    pub width: u32,
    pub height: u32,
    /// `(channel, message) → control index`; a pad's pressure is found
    /// through its note.
    index: HashMap<(u8, Message), usize>,
}

impl Surface {
    /// Parses a surface file.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let file: SurfaceFile = serde_json::from_str(json).map_err(|err| err.to_string())?;
        Self::from_file(file)
    }

    fn from_file(file: SurfaceFile) -> Result<Self, String> {
        let mut banks = Vec::new();
        let mut controls = Vec::new();
        for (bank_index, bank) in file.banks.iter().enumerate() {
            if bank.columns == 0 || bank.rows == 0 {
                return Err(format!("bank {}: no rows or columns", bank.name));
            }
            let capacity = (bank.rows * bank.columns) as usize;
            if bank.controls.len() > capacity {
                return Err(format!(
                    "bank {}: {} controls in {}×{}",
                    bank.name,
                    bank.controls.len(),
                    bank.rows,
                    bank.columns
                ));
            }
            let (cell_width, cell_height) = bank.kind.cell_size();
            for (number, control) in bank.controls.iter().enumerate() {
                let message = control.message().map_err(|err| format!("bank {}: {err}", bank.name))?;
                let channel = control.channel.unwrap_or(bank.channel);
                if !(1..=16).contains(&channel) {
                    return Err(format!("bank {}: channel {channel} isn't 1–16", bank.name));
                }
                let (row, column) = cell(number as u32, bank.rows, bank.columns, bank.numbering);
                controls.push(Control {
                    kind: bank.kind,
                    label: control
                        .label
                        .clone()
                        .unwrap_or_else(|| format!("{}{}", bank.prefix, number + 1)),
                    message,
                    channel: channel - 1,
                    pressure: bank.pressure.filter(|_| bank.kind == ControlKind::Pad),
                    x: bank.x + column * cell_width,
                    y: bank.y + row * cell_height,
                    bank: bank_index,
                });
            }
            banks.push(Bank {
                name: bank.name.clone(),
                kind: bank.kind,
                x: bank.x,
                y: bank.y,
                width: bank.columns * cell_width,
                height: bank.rows * cell_height,
            });
        }
        let width = banks.iter().map(|bank| bank.x + bank.width).max().unwrap_or(0);
        let height = banks.iter().map(|bank| bank.y + bank.height).max().unwrap_or(0);
        let mut index = HashMap::new();
        for (control_index, control) in controls.iter().enumerate() {
            if index.insert((control.channel, control.message), control_index).is_some() {
                return Err(format!(
                    "two controls send {:?} on channel {}",
                    control.message,
                    control.channel + 1
                ));
            }
        }
        Ok(Self {
            id: file.id,
            name: file.name,
            devices: file.devices,
            banks,
            controls,
            width,
            height,
            index,
        })
    }

    /// Whether this is the surface for MIDI device `device` (a
    /// `midi_device_name`).
    pub fn matches(&self, device: &str) -> bool {
        let device = device.to_lowercase();
        self.devices.iter().any(|pattern| device.contains(&pattern.to_lowercase()))
    }

    /// The control `event` comes from, and what it did; `None` for anything
    /// else (keys, other channels…).
    pub fn read(&self, event: &MidiEvent) -> Option<(usize, Reading)> {
        let bytes = event.as_slice();
        let status = *bytes.first()?;
        let channel = status & 0x0F;
        let data = |index: usize| bytes.get(index).copied().unwrap_or(0);
        let find = |message| self.index.get(&(channel, message)).copied();
        match status & 0xF0 {
            0x90 if data(2) > 0 => {
                let control = find(Message::Note(data(1)))?;
                Some((control, Reading::Press(f32::from(data(2)) / 127.0)))
            }
            0x80 | 0x90 => Some((find(Message::Note(data(1)))?, Reading::Release)),
            0xB0 => {
                let control = find(Message::Cc(data(1)))?;
                let value = f32::from(data(2)) / 127.0;
                Some((control, match self.controls[control].kind {
                    ControlKind::Fader | ControlKind::Knob => Reading::Value(value),
                    ControlKind::Pad | ControlKind::Button if data(2) > 63 => Reading::Press(1.0),
                    ControlKind::Pad | ControlKind::Button => Reading::Release,
                }))
            }
            0xE0 => {
                let value = u16::from(data(1)) | u16::from(data(2)) << 7;
                Some((find(Message::PitchBend)?, Reading::Value(f32::from(value) / 16383.0)))
            }
            // A pad's channel pressure belongs to whichever pad is held, which
            // the caller knows: see `Surface::pad_pressure`.
            0xD0 => Some((find(Message::ChannelPressure)?, Reading::Value(f32::from(data(1)) / 127.0))),
            0xA0 => {
                let control = find(Message::Note(data(1)))?;
                (self.controls[control].pressure == Some(Pressure::Poly))
                    .then_some((control, Reading::Value(f32::from(data(2)) / 127.0)))
            }
            _ => None,
        }
    }

    /// For a channel-pressure message that isn't a control of its own: the
    /// pads it may belong to (those sending channel pressure on its channel —
    /// it's the held one's) and the pressure, 0..=1.
    pub fn pad_pressure(&self, event: &MidiEvent) -> Option<(Vec<usize>, f32)> {
        let bytes = event.as_slice();
        let (&status, &value) = (bytes.first()?, bytes.get(1)?);
        let channel = status & 0x0F;
        if status & 0xF0 != 0xD0 || self.index.contains_key(&(channel, Message::ChannelPressure)) {
            return None;
        }
        let pads: Vec<usize> = self
            .controls
            .iter()
            .enumerate()
            .filter(|(_, control)| control.channel == channel && control.pressure == Some(Pressure::Channel))
            .map(|(index, _)| index)
            .collect();
        (!pads.is_empty()).then_some((pads, f32::from(value) / 127.0))
    }
}

/// Row and column of control `number` (0-based) in a `rows` × `columns` bank.
fn cell(number: u32, rows: u32, columns: u32, numbering: Numbering) -> (u32, u32) {
    let row = number / columns;
    let row = match numbering {
        Numbering::TopFirst => row,
        Numbering::BottomFirst => rows - 1 - row,
    };
    (row, number % columns)
}

/// Every surface known: the user's, then the shipped ones they don't replace.
#[derive(Debug, Clone, Default)]
pub struct Library {
    pub surfaces: Vec<Surface>,
}

impl Library {
    /// Loads the shipped surfaces and the user's.
    pub fn load() -> Self {
        let mut surfaces: Vec<Surface> = user_dir().map(|dir| read_dir(&dir)).unwrap_or_default();
        for (name, json) in SHIPPED {
            match Surface::from_json(json) {
                Ok(surface) if surfaces.iter().any(|known| known.id == surface.id) => {}
                Ok(surface) => surfaces.push(surface),
                // A test catches this; never fatal.
                Err(err) => log::error!("shipped surface {name}: {err}"),
            }
        }
        Self { surfaces }
    }

    /// The surface for MIDI device `device` (a `midi_device_name`), if any.
    pub fn for_device(&self, device: &str) -> Option<&Surface> {
        self.surfaces.iter().find(|surface| surface.matches(device))
    }
}

/// Where the user's surfaces live: `~/.local/share/live-keys/surfaces` on Linux.
pub fn user_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("live-keys").join("surfaces"))
}

fn read_dir(dir: &Path) -> Vec<Surface> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect();
    paths.sort();
    let mut surfaces = Vec::new();
    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|json| Surface::from_json(&json));
        match parsed {
            Ok(surface) => surfaces.push(surface),
            Err(err) => log::warn!("surface {}: {err}", path.display()),
        }
    }
    surfaces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(bytes: &[u8]) -> MidiEvent {
        MidiEvent::from_raw(0, bytes).unwrap()
    }

    #[test]
    fn shipped_surfaces_parse() {
        for (name, json) in SHIPPED {
            if let Err(err) = Surface::from_json(json) {
                panic!("{name}: {err}");
            }
        }
    }

    #[test]
    fn bottom_first_numbering_starts_on_the_last_row() {
        let lpd8 = Surface::from_json(SHIPPED[1].1).unwrap();
        let pad = |label: &str| lpd8.controls.iter().find(|control| control.label == label).unwrap();
        let (_, pad_height) = ControlKind::Pad.cell_size();
        assert_eq!((pad("P1").x, pad("P1").y), (0, pad_height));
        assert_eq!((pad("P5").x, pad("P5").y), (0, 0));
        assert_eq!(pad("P8").x, 3 * ControlKind::Pad.cell_size().0);
    }

    #[test]
    fn events_are_read_per_control_kind() {
        let lx = Surface::from_json(SHIPPED[0].1).unwrap();
        let label = |index: usize| lx.controls[index].label.as_str();
        // F1 (CC 80, channel 1), halfway.
        let (control, reading) = lx.read(&event(&[0xB0, 80, 127])).unwrap();
        assert_eq!((label(control), reading), ("F1", Reading::Value(1.0)));
        // P1 (note 36, channel 10) pressed and let go, as note-on with velocity 0.
        let (control, reading) = lx.read(&event(&[0x99, 36, 127])).unwrap();
        assert_eq!((label(control), reading), ("P1", Reading::Press(1.0)));
        assert_eq!(lx.read(&event(&[0x99, 36, 0])).unwrap().1, Reading::Release);
        // Pitch bend at rest is the middle.
        let (control, reading) = lx.read(&event(&[0xE0, 0, 64])).unwrap();
        let Reading::Value(value) = reading else { panic!() };
        assert_eq!(label(control), "Pitch");
        assert!((value - 0.5).abs() < 0.001);
        // Keys and other channels aren't controls.
        assert!(lx.read(&event(&[0x90, 60, 100])).is_none());
        assert!(lx.read(&event(&[0xB1, 80, 1])).is_none());
    }

    #[test]
    fn a_control_needs_exactly_one_message() {
        let json = |control: &str| {
            format!(r#"{{ "id": "x", "name": "x", "banks": [{{ "name": "b", "kind": "knob", "columns": 1, "controls": [{control}] }}] }}"#)
        };
        assert!(Surface::from_json(&json(r#"{ "cc": 1 }"#)).is_ok());
        assert!(Surface::from_json(&json(r#"{ }"#)).is_err());
        assert!(Surface::from_json(&json(r#"{ "cc": 1, "note": 2 }"#)).is_err());
        assert!(Surface::from_json(&json(r#"{ "cc": 128 }"#)).is_err());
    }
}
