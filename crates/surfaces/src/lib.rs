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
//! with its own `channel` and `label`. A bank (or a control, overriding it)
//! with `"feedback": true` can show a value sent back as its own message —
//! motor faders, LED rings, lit pads and buttons. That's all a surface says
//! about lights: what one shows (which target, inverted, its lit value or
//! colour) is each mapping's choice.
//!
//! **Several ports, pages, shift** (a controller like the Nektar Impact LX88+):
//! - `ports`: one pattern per MIDI port of the device, in port order (a
//!   substring of the port's device name; `a|b` for alternatives). A port's
//!   device name without its pattern is the device the ports make together
//!   ("Impact LX88+:Impact LX88+ MIDI2" → port 2 of "Impact LX88+:Impact
//!   LX88+"). A bank's `port` (default 1) is the one its messages come on.
//!   `claim-ports`: ports whose every event belongs to the surface, mapped or
//!   not (never reaching the slots). `output-port` (default 1): the port
//!   feedback is sent to.
//! - `pages`: `{ "id", "name", "button", "variants" }` — sets of banks drawn in
//!   the same place, one at a time; a bank's `page` is a page's `id` (none:
//!   always there). A page's controls are controls of their own, labelled
//!   "<page name> <label>". The page shown follows the hardware: its `button`
//!   (a control's label) pressed, or a message only that page's controls
//!   send. `variants` names the page's alternative messages (a keyboard's
//!   presets): a bank's `controls` are the first variant's, its `variants`
//!   (`[{ "channel", "controls" }]`) the others', in order; which one is read
//!   is chosen by the user ([`Options`]); a variant can also have its own
//!   `toggle`.
//! - A control with `"shift": true` is the surface's shift button; one with
//!   `"shifted": "<label>"` has a twin of that label: its messages while shift
//!   is held are the twin's ([`Reader`]).
//! - A button bank with `"toggle": true` sends a CC that the hardware latches
//!   (127, then 0 on the next press): every message is a press.
//! - A bank's `cell-width`/`cell-height` size its controls' cells, if not
//!   as their kind's ([`ControlKind::cell_size`]).
//! - `defaults`: mappings the app installs the first time the surface is
//!   shown (the GUI's mapping format; `control` is a control's label).
//! - `driver`: the name of code that talks to the device beyond plain
//!   messages (a handshake, a display): see the `driver` module.

pub mod driver;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use live_keys_core::MidiEvent;
use serde::{Deserialize, Serialize};

/// The shipped surfaces, by file name.
///
/// Their MIDI messages come from the makers' published defaults where those
/// could be found (the LX88+'s faders and pots) or the hardware (the original
/// LPD8, pad lights included); the LX88+'s pads are a best guess still to be
/// checked.
pub const SHIPPED: &[(&str, &str)] = &[
    ("nektar-impact-lx88plus.json", include_str!("../surfaces/nektar-impact-lx88plus.json")),
    ("akai-lpd8.json", include_str!("../surfaces/akai-lpd8.json")),
];

/// The kinds of control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Numbering {
    #[default]
    TopFirst,
    BottomFirst,
}

/// What a pad sends while held, besides its note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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

/// A surface file as written (see the format above): what the editor edits.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SurfaceSpec {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_ports: Vec<u8>,
    #[serde(default = "one_u8", skip_serializing_if = "is_one")]
    pub output_port: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PageSpec>,
    pub banks: Vec<BankSpec>,
    /// Mappings to start with: the GUI's mapping format, with `control` a
    /// control's label (see the GUI's `MappingsUi::install_defaults`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defaults: Vec<serde_json::Value>,
}

/// A page as written.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PageSpec {
    pub id: String,
    pub name: String,
    /// The label of the control that switches to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
}

/// One of a page's other variants of a bank's messages.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VariantSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toggle: Option<bool>,
    pub controls: Vec<ControlSpec>,
}

/// A bank as written.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct BankSpec {
    pub name: String,
    pub kind: ControlKind,
    #[serde(default)]
    pub x: u32,
    #[serde(default)]
    pub y: u32,
    #[serde(default = "one")]
    pub rows: u32,
    pub columns: u32,
    #[serde(default, skip_serializing_if = "is_default")]
    pub numbering: Numbering,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(default = "one_u8")]
    pub channel: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<Pressure>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub feedback: bool,
    #[serde(default = "one_u8", skip_serializing_if = "is_one")]
    pub port: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub toggle: bool,
    /// Its controls' cell size, if not their kind's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_height: Option<u32>,
    pub controls: Vec<ControlSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<VariantSpec>,
}

/// A control as written: one message, and what it does differently from its
/// bank.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ControlSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<u8>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub pitch_bend: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub channel_pressure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<bool>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub shift: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shifted: Option<String>,
}

fn is_one(value: &u8) -> bool {
    *value == 1
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl ControlSpec {
    /// A control sending `message`.
    pub fn of(message: Message) -> Self {
        let mut spec = Self::default();
        match message {
            Message::Cc(number) => spec.cc = Some(number),
            Message::Note(number) => spec.note = Some(number),
            Message::PitchBend => spec.pitch_bend = true,
            Message::ChannelPressure => spec.channel_pressure = true,
        }
        spec
    }
}

fn one() -> u32 {
    1
}

fn one_u8() -> u8 {
    1
}

impl ControlSpec {
    /// Its message, or why it hasn't exactly one.
    pub fn message(&self) -> Result<Message, String> {
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
    /// Unique in its surface (mappings name controls by it): a page's
    /// controls have the page's name in front.
    pub label: String,
    /// What the panel writes on it.
    pub short: String,
    pub message: Message,
    /// 0..=15.
    pub channel: u8,
    /// The device's port it's on, 1-based.
    pub port: u8,
    /// A pad's pressure messages.
    pub pressure: Option<Pressure>,
    /// Its cell in the surface, in pixels.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Its bank, as an index into [`Surface::banks`].
    pub bank: usize,
    /// Its page, as an index into [`Surface::pages`].
    pub page: Option<usize>,
    /// It shows a value sent back to it, as its own message.
    pub feedback: bool,
    /// A button whose CC the hardware latches: every message is a press.
    pub toggle: bool,
    /// Its twin while shift is held.
    pub twin: Option<usize>,
    /// It's the shifted twin of that control (it has no message of its own,
    /// and is drawn in its place while shift is held).
    pub twin_of: Option<usize>,
}

/// A value to show on a control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shown {
    /// A fader's or knob's position, 0..=1.
    Position(f32),
    /// A pad or button lit, or not.
    Lit(bool),
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
    pub page: Option<usize>,
}

/// A page: banks drawn in the same place as other pages', one at a time.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub id: String,
    pub name: String,
    /// The control that switches to it.
    pub button: Option<usize>,
    /// Its variants' names (none: it has one).
    pub variants: Vec<String>,
}

/// How a surface is read, beyond its file: chosen by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Per page id, the variant read (default the first).
    pub variants: BTreeMap<String, usize>,
    /// Every message arrives on port 1: the device isn't in the mode where it
    /// spreads them over its ports (its driver is off).
    pub one_port: bool,
}

/// A surface, its controls placed and indexed by message.
#[derive(Debug, Clone)]
pub struct Surface {
    pub id: String,
    pub name: String,
    pub devices: Vec<String>,
    /// Port patterns, see the format.
    pub ports: Vec<String>,
    pub claim_ports: Vec<u8>,
    pub output_port: u8,
    pub driver: Option<String>,
    pub pages: Vec<Page>,
    pub banks: Vec<Bank>,
    pub controls: Vec<Control>,
    /// The shift button.
    pub shift: Option<usize>,
    /// Size of the whole drawing, in pixels.
    pub width: u32,
    pub height: u32,
    /// `(port, channel, message) → control indices` (more than one only on
    /// different pages, or within a variant); a pad's pressure is found
    /// through its note.
    index: HashMap<(u8, u8, Message), Vec<usize>>,
    /// What it was made from.
    pub spec: SurfaceSpec,
    /// How it was read from it.
    pub options: Options,
    /// It's the user's (a file in [`user_dir`]), not a shipped one.
    pub user: bool,
}

impl Surface {
    /// Parses a surface file.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let file: SurfaceSpec = serde_json::from_str(json).map_err(|err| err.to_string())?;
        Self::from_spec(file)
    }

    /// Lays out `file`'s controls; an error says what's wrong with it.
    pub fn from_spec(file: SurfaceSpec) -> Result<Self, String> {
        Self::resolve(file, Options::default())
    }

    /// The same surface read with other options.
    pub fn with_options(&self, options: Options) -> Self {
        let mut surface = Self::resolve(self.spec.clone(), options).unwrap_or_else(|_| self.clone());
        surface.user = self.user;
        surface
    }

    fn resolve(file: SurfaceSpec, options: Options) -> Result<Self, String> {
        let mut pages: Vec<Page> = Vec::new();
        for page in &file.pages {
            if pages.iter().any(|known| known.id == page.id) {
                return Err(format!("two pages are called {}", page.id));
            }
            pages.push(Page { id: page.id.clone(), name: page.name.clone(), button: None, variants: page.variants.clone() });
        }
        let mut banks = Vec::new();
        let mut controls = Vec::new();
        let mut twins: Vec<(usize, String)> = Vec::new();
        let mut shifts: Vec<usize> = Vec::new();
        for (bank_index, bank) in file.banks.iter().enumerate() {
            if bank.columns == 0 || bank.rows == 0 {
                return Err(format!("bank {}: no rows or columns", bank.name));
            }
            let page = match &bank.page {
                Some(id) => Some(
                    pages
                        .iter()
                        .position(|page| page.id == *id)
                        .ok_or_else(|| format!("bank {}: no page {id}", bank.name))?,
                ),
                None => None,
            };
            let variant = page.and_then(|page| options.variants.get(&pages[page].id).copied()).unwrap_or(0);
            let (bank_channel, toggle, specs) = match variant.checked_sub(1).and_then(|other| bank.variants.get(other)) {
                Some(other) => (other.channel.unwrap_or(bank.channel), other.toggle.unwrap_or(bank.toggle), &other.controls),
                None => (bank.channel, bank.toggle, &bank.controls),
            };
            let capacity = (bank.rows * bank.columns) as usize;
            if bank.controls.len() > capacity || bank.variants.iter().any(|other| other.controls.len() > capacity) {
                return Err(format!("bank {}: more controls than {}×{}", bank.name, bank.rows, bank.columns));
            }
            if !(1..=16).contains(&bank.port) {
                return Err(format!("bank {}: port {} isn't 1–16", bank.name, bank.port));
            }
            let (kind_width, kind_height) = bank.kind.cell_size();
            let (cell_width, cell_height) = (bank.cell_width.unwrap_or(kind_width), bank.cell_height.unwrap_or(kind_height));
            if cell_width < 16 || cell_height < 16 {
                return Err(format!("bank {}: cells under 16 px", bank.name));
            }
            for (number, control) in specs.iter().enumerate() {
                let message = control.message().map_err(|err| format!("bank {}: {err}", bank.name))?;
                let channel = control.channel.unwrap_or(bank_channel);
                if !(1..=16).contains(&channel) {
                    return Err(format!("bank {}: channel {channel} isn't 1–16", bank.name));
                }
                // Labels and feedback are the first variant's.
                let first = bank.controls.get(number).unwrap_or(control);
                let (row, column) = cell(number as u32, bank.rows, bank.columns, bank.numbering);
                let short = first.label.clone().unwrap_or_else(|| format!("{}{}", bank.prefix, number + 1));
                let label = match page {
                    Some(page) => format!("{} {short}", pages[page].name),
                    None => short.clone(),
                };
                if first.shift {
                    shifts.push(controls.len());
                }
                if let Some(twin) = &first.shifted {
                    twins.push((controls.len(), twin.clone()));
                }
                controls.push(Control {
                    kind: bank.kind,
                    label,
                    short,
                    message,
                    channel: channel - 1,
                    port: if options.one_port { 1 } else { bank.port },
                    pressure: bank.pressure.filter(|_| bank.kind == ControlKind::Pad),
                    x: bank.x + column * cell_width,
                    y: bank.y + row * cell_height,
                    width: cell_width,
                    height: cell_height,
                    bank: bank_index,
                    page,
                    feedback: first.feedback.unwrap_or(bank.feedback),
                    toggle: toggle && bank.kind == ControlKind::Button,
                    twin: None,
                    twin_of: None,
                });
            }
            banks.push(Bank {
                name: bank.name.clone(),
                kind: bank.kind,
                x: bank.x,
                y: bank.y,
                width: bank.columns * cell_width,
                height: bank.rows * cell_height,
                page,
            });
        }
        // Shifted twins come after every other control, so indices of the
        // others don't depend on them.
        for (of, label) in twins {
            let original = &controls[of];
            let twin = Control {
                label: label.clone(),
                short: label,
                twin: None,
                twin_of: Some(of),
                feedback: false,
                ..original.clone()
            };
            let index = controls.len();
            controls[of].twin = Some(index);
            controls.push(twin);
        }
        if shifts.len() > 1 {
            return Err("more than one shift button".into());
        }
        let shift = shifts.first().copied();
        for (spec, page) in file.pages.iter().zip(pages.iter_mut()) {
            if let Some(button) = &spec.button {
                page.button = Some(
                    controls
                        .iter()
                        .position(|control| control.label == *button)
                        .ok_or_else(|| format!("page {}: no control {button}", spec.id))?,
                );
            }
        }
        for port in file.claim_ports.iter().chain([&file.output_port]) {
            if !(1..=16).contains(port) {
                return Err(format!("port {port} isn't 1–16"));
            }
        }
        let width = banks.iter().map(|bank| bank.x + bank.width).max().unwrap_or(0);
        let height = banks.iter().map(|bank| bank.y + bank.height).max().unwrap_or(0);
        // Mappings name controls by label.
        let mut labels = HashSet::new();
        if let Some(control) = controls.iter().find(|control| !labels.insert(control.label.as_str())) {
            return Err(format!("two controls are called {}", control.label));
        }
        let mut index: HashMap<(u8, u8, Message), Vec<usize>> = HashMap::new();
        for (control_index, control) in controls.iter().enumerate() {
            if control.twin_of.is_some() {
                continue;
            }
            let entry = index.entry((control.port, control.channel, control.message)).or_default();
            // Two controls of one page (or both of none) may only share a
            // message in another variant than the first, where the hardware
            // does that (the LX88+'s preset 2 has its F1 and F9 on CC 7).
            let clash = entry.iter().any(|&other| controls[other].page == control.page);
            let variant = control.page.is_some_and(|page| options.variants.get(&pages[page].id).is_some_and(|&v| v > 0));
            if clash && !variant && !options.one_port {
                return Err(format!(
                    "two controls send {:?} on channel {}",
                    control.message,
                    control.channel + 1
                ));
            }
            entry.push(control_index);
        }
        Ok(Self {
            id: file.id.clone(),
            name: file.name.clone(),
            devices: file.devices.clone(),
            ports: file.ports.clone(),
            claim_ports: file.claim_ports.clone(),
            output_port: file.output_port,
            driver: file.driver.clone(),
            pages,
            banks,
            controls,
            shift,
            width,
            height,
            index,
            spec: file,
            options,
            user: false,
        })
    }

    /// Whether this is the surface for MIDI device `device` (a
    /// `midi_device_name`, or a device its ports make, see [`Surface::port_of`]).
    pub fn matches(&self, device: &str) -> bool {
        let device = device.to_lowercase();
        self.devices.iter().any(|pattern| device.contains(&pattern.to_lowercase()))
    }

    /// For the MIDI port named `port` (a `midi_device_name`), if it's one of
    /// this surface's: the device its ports make together, and its number
    /// (1-based). A surface without `ports` has one, named as the device.
    pub fn port_of(&self, port: &str) -> Option<(String, u8)> {
        if !self.matches(port) {
            return None;
        }
        let lower = port.to_lowercase();
        for (number, pattern) in self.ports.iter().enumerate() {
            for alternative in pattern.split('|').filter(|alternative| !alternative.is_empty()) {
                if let Some(at) = lower.find(&alternative.to_lowercase()) {
                    // Lowercasing can change lengths outside ASCII: then keep
                    // the name whole.
                    let device = if lower.len() == port.len() {
                        format!("{}{}", &port[..at], &port[at + alternative.len()..])
                    } else {
                        port.to_string()
                    };
                    let device = device.trim_matches(|c: char| c.is_whitespace() || "()-_".contains(c));
                    return Some((device.to_string(), number as u8 + 1));
                }
            }
        }
        Some((port.to_string(), 1))
    }

    /// The control `event` (arriving on port `port`) comes from, preferring
    /// one on page `page`, and what it did; `None` for anything else (keys,
    /// other channels…). Shift is up to the caller: see [`Reader`].
    pub fn read(&self, port: u8, page: Option<usize>, event: &MidiEvent) -> Option<(usize, Reading)> {
        let port = if self.options.one_port { 1 } else { port };
        let bytes = event.as_slice();
        let status = *bytes.first()?;
        let channel = status & 0x0F;
        let data = |index: usize| bytes.get(index).copied().unwrap_or(0);
        let find = |message| {
            let found = self.index.get(&(port, channel, message))?;
            found
                .iter()
                .copied()
                .find(|&control| self.controls[control].page == page)
                .or_else(|| found.iter().copied().find(|&control| self.controls[control].page.is_none()))
                .or_else(|| found.first().copied())
        };
        match status & 0xF0 {
            0x90 if data(2) > 0 => {
                let control = find(Message::Note(data(1)))?;
                Some((control, Reading::Press(f32::from(data(2)) / 127.0)))
            }
            0x80 | 0x90 => Some((find(Message::Note(data(1)))?, Reading::Release)),
            0xB0 => {
                let control = find(Message::Cc(data(1)))?;
                let value = f32::from(data(2)) / 127.0;
                let found = &self.controls[control];
                Some((control, match found.kind {
                    ControlKind::Fader | ControlKind::Knob => Reading::Value(value),
                    ControlKind::Button if found.toggle => Reading::Press(1.0),
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

    /// The message showing `shown` on control `control`, if it takes feedback
    /// (and its message can carry a value: not channel pressure). `lit` is
    /// "on" for a note or a CC lit: the velocity or value (some devices pick
    /// a colour by it).
    pub fn feedback(&self, control: usize, shown: Shown, lit: u8) -> Option<[u8; 3]> {
        let control = self.controls.get(control)?;
        if !control.feedback {
            return None;
        }
        let lit = lit.clamp(1, 127);
        let channel = control.channel & 0x0F;
        let seven = |position: f32| (position.clamp(0.0, 1.0) * 127.0).round() as u8;
        Some(match (control.message, shown) {
            (Message::Cc(number), Shown::Position(position)) => [0xB0 | channel, number, seven(position)],
            (Message::Cc(number), Shown::Lit(on)) => [0xB0 | channel, number, if on { lit } else { 0 }],
            (Message::Note(number), shown) => {
                let on = match shown {
                    Shown::Lit(on) => on,
                    Shown::Position(position) => position > 0.0,
                };
                // Off as a note-on with velocity 0, MIDI's other note-off: the
                // Nektar Impact LX+'s pads ignore a real note-off (tested).
                [0x90 | channel, number, if on { lit } else { 0 }]
            }
            (Message::PitchBend, shown) => {
                let position = match shown {
                    Shown::Position(position) => position,
                    Shown::Lit(on) => f32::from(u8::from(on)),
                };
                let value = (position.clamp(0.0, 1.0) * 16383.0).round() as u16;
                [0xE0 | channel, (value & 0x7F) as u8, (value >> 7) as u8]
            }
            (Message::ChannelPressure, _) => return None,
        })
    }

    /// For a channel-pressure message that isn't a control of its own: the
    /// pads it may belong to (those sending channel pressure on its port and
    /// channel — it's the held one's) and the pressure, 0..=1.
    pub fn pad_pressure(&self, port: u8, event: &MidiEvent) -> Option<(Vec<usize>, f32)> {
        let port = if self.options.one_port { 1 } else { port };
        let bytes = event.as_slice();
        let (&status, &value) = (bytes.first()?, bytes.get(1)?);
        let channel = status & 0x0F;
        if status & 0xF0 != 0xD0 || self.index.contains_key(&(port, channel, Message::ChannelPressure)) {
            return None;
        }
        let pads: Vec<usize> = self
            .controls
            .iter()
            .enumerate()
            .filter(|(_, control)| {
                control.port == port
                    && control.channel == channel
                    && control.pressure == Some(Pressure::Channel)
                    && control.twin_of.is_none()
            })
            .map(|(index, _)| index)
            .collect();
        (!pads.is_empty()).then_some((pads, f32::from(value) / 127.0))
    }

    /// Whether control `control` is drawn on page `page` with shift `shift`:
    /// a page's controls on their page only, a shifted twin in its
    /// original's place while shift is held.
    pub fn visible(&self, control: usize, page: Option<usize>, shift: bool) -> bool {
        let Some(found) = self.controls.get(control) else { return false };
        let on_page = found.page.is_none() || found.page == page;
        let shifted = match (found.twin, found.twin_of) {
            (_, Some(_)) => shift,
            (Some(_), None) => !shift,
            (None, None) => true,
        };
        on_page && shifted
    }

    /// The page shown before anything was read: the first, if there are any.
    pub fn first_page(&self) -> Option<usize> {
        (!self.pages.is_empty()).then_some(0)
    }
}

/// What a surface's hardware is doing beyond single messages — the page it's
/// on, shift held —, kept per device by whoever reads its events (the mapping
/// thread, the panel), with the [`Surface`] they read it with.
#[derive(Debug, Clone, Default)]
pub struct Reader {
    page: Option<usize>,
    shift: bool,
    /// Controls pressed while shift was held: their release is their twin's.
    shifted: HashSet<usize>,
}

/// What one event did, as [`Reader::read`] says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Read {
    /// The control it came from (a shifted twin while shift is held), and
    /// what it did.
    pub control: Option<(usize, Reading)>,
    /// Channel pressure that belongs to a held pad: the pads it may be, and
    /// the pressure.
    pub pressure: Option<(Vec<usize>, f32)>,
    /// The page shown changed (to [`Reader::page`]).
    pub page_changed: bool,
}

impl Reader {
    /// For `surface`, on its first page.
    pub fn new(surface: &Surface) -> Self {
        Self { page: surface.first_page(), ..Self::default() }
    }

    pub fn page(&self) -> Option<usize> {
        self.page
    }

    /// Shows page `page` (the user picked it), whatever the hardware is on.
    pub fn set_page(&mut self, page: Option<usize>) {
        self.page = page;
    }

    pub fn shift(&self) -> bool {
        self.shift
    }

    /// Reads `event`, from `surface`'s port `port`.
    pub fn read(&mut self, surface: &Surface, port: u8, event: &MidiEvent) -> Read {
        let mut read = Read::default();
        let Some((control, reading)) = surface.read(port, self.page, event) else {
            read.pressure = surface.pad_pressure(port, event);
            return read;
        };
        let found = &surface.controls[control];
        let pressed = matches!(reading, Reading::Press(_));
        if surface.shift == Some(control) {
            self.shift = pressed;
        }
        let page = match found.page {
            Some(page) => Some(page),
            None if pressed => surface.pages.iter().position(|page| page.button == Some(control)),
            None => None,
        };
        if let Some(page) = page
            && self.page != Some(page)
        {
            self.page = Some(page);
            read.page_changed = true;
        }
        let control = match (found.twin, reading) {
            (Some(twin), Reading::Release) if self.shifted.remove(&control) => twin,
            (Some(twin), Reading::Press(_)) if self.shift => {
                self.shifted.insert(control);
                twin
            }
            (Some(twin), Reading::Value(_)) if self.shift => twin,
            _ => control,
        };
        read.control = Some((control, reading));
        read
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

    /// Saves `spec` as the user's surface (`<id>.json` in [`user_dir`]),
    /// replacing one with that id; it must lay out.
    pub fn save(spec: &SurfaceSpec) -> Result<PathBuf, String> {
        Surface::from_spec(spec.clone())?;
        let dir = user_dir().ok_or("no data folder")?;
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        // Whatever file had it before.
        Self::delete(&spec.id)?;
        let path = dir.join(format!("{}.json", spec.id));
        let json = serde_json::to_string_pretty(spec).map_err(|err| err.to_string())?;
        std::fs::write(&path, json + "\n").map_err(|err| err.to_string())?;
        Ok(path)
    }

    /// Deletes the user's surfaces with id `id` (a shipped one with that id
    /// comes back).
    pub fn delete(id: &str) -> Result<(), String> {
        let Some(dir) = user_dir() else { return Ok(()) };
        let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(()) };
        for path in entries.flatten().map(|entry| entry.path()) {
            let is_it = std::fs::read_to_string(&path)
                .ok()
                .and_then(|json| serde_json::from_str::<SurfaceSpec>(&json).ok())
                .is_some_and(|spec| spec.id == id);
            if is_it {
                std::fs::remove_file(&path).map_err(|err| err.to_string())?;
            }
        }
        Ok(())
    }

    /// The surface for MIDI device `device` (a `midi_device_name`, or a
    /// device several ports make), if any.
    pub fn for_device(&self, device: &str) -> Option<&Surface> {
        self.surfaces.iter().find(|surface| surface.matches(device))
    }

    /// The device MIDI port `port` (a `midi_device_name`) belongs to, and its
    /// number there: its surface's say (see [`Surface::port_of`]), or the port
    /// alone, as port 1.
    pub fn device_of(&self, port: &str) -> (String, u8) {
        self.surfaces
            .iter()
            .find_map(|surface| surface.port_of(port))
            .unwrap_or_else(|| (port.to_string(), 1))
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
            Ok(mut surface) => {
                surface.user = true;
                surfaces.push(surface);
            }
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
        assert_eq!(lpd8.id, "akai-lpd8");
        let pad = |label: &str| lpd8.controls.iter().find(|control| control.label == label).unwrap();
        let (_, pad_height) = ControlKind::Pad.cell_size();
        assert_eq!((pad("P1").x, pad("P1").y), (0, pad_height));
        assert_eq!((pad("P5").x, pad("P5").y), (0, 0));
        assert_eq!(pad("P8").x, 3 * ControlKind::Pad.cell_size().0);
    }

    fn lx() -> Surface {
        Surface::from_json(SHIPPED[0].1).unwrap()
    }

    fn label(surface: &Surface, read: Option<(usize, Reading)>) -> (&str, Reading) {
        let (control, reading) = read.unwrap();
        (surface.controls[control].label.as_str(), reading)
    }

    #[test]
    fn events_are_read_per_control_kind_and_port() {
        let lx = lx();
        // Preset 1's F1 (CC 73, channel 1, port 1), at the top.
        assert_eq!(label(&lx, lx.read(1, None, &event(&[0xB0, 73, 127]))), ("Preset F1", Reading::Value(1.0)));
        // P1 (note 60, channel 16, port 2) pressed and let go, as note-on with velocity 0.
        assert_eq!(label(&lx, lx.read(2, None, &event(&[0x9F, 60, 127]))), ("P1", Reading::Press(1.0)));
        assert_eq!(lx.read(2, None, &event(&[0x9F, 60, 0])).unwrap().1, Reading::Release);
        // Pitch bend at rest is the middle.
        let (name, reading) = label(&lx, lx.read(1, None, &event(&[0xE0, 0, 64])));
        let Reading::Value(value) = reading else { panic!() };
        assert_eq!(name, "Pitch");
        assert!((value - 0.5).abs() < 0.001);
        // Keys, other channels and other ports aren't controls.
        assert!(lx.read(1, None, &event(&[0x90, 60, 100])).is_none());
        assert!(lx.read(1, None, &event(&[0xB1, 73, 1])).is_none());
        assert!(lx.read(1, None, &event(&[0x9F, 60, 100])).is_none());
        // A latching button: its "off" is a press too.
        assert_eq!(label(&lx, lx.read(1, None, &event(&[0xB0, 0, 0]))), ("Preset B1", Reading::Press(1.0)));
    }

    #[test]
    fn ports_make_one_device() {
        let lx = lx();
        let device = "Impact LX88+:Impact LX88+".to_string();
        assert_eq!(lx.port_of("Impact LX88+:Impact LX88+ MIDI1"), Some((device.clone(), 1)));
        assert_eq!(lx.port_of("Impact LX88+:Impact LX88+ MIDI2"), Some((device, 2)));
        assert_eq!(lx.port_of("LPD8:LPD8 MIDI 1"), None);
        let library = Library { surfaces: vec![lx] };
        assert_eq!(library.device_of("LPD8:LPD8 MIDI 1"), ("LPD8:LPD8 MIDI 1".to_string(), 1));
    }

    #[test]
    fn the_page_follows_the_hardware() {
        let lx = lx();
        let mut reader = Reader::new(&lx);
        let page = |reader: &Reader| reader.page().map(|page| lx.pages[page].id.as_str());
        assert_eq!(page(&reader), Some("mixer"));
        // An Inst fader moves: the Inst page shows.
        let read = reader.read(&lx, 2, &event(&[0xBF, 38, 10]));
        assert!(read.page_changed);
        assert_eq!(label(&lx, read.control).0, "Inst F1");
        assert_eq!(page(&reader), Some("inst"));
        // The Preset button.
        let read = reader.read(&lx, 2, &event(&[0xBF, 115, 127]));
        assert!(read.page_changed);
        assert_eq!(label(&lx, read.control).0, "Preset");
        assert_eq!(page(&reader), Some("preset"));
        assert!(!reader.read(&lx, 2, &event(&[0xBF, 115, 0])).page_changed);
    }

    #[test]
    fn variants_change_what_a_page_reads() {
        let lx = lx();
        // Preset 1: CC 7 on channel 1 is F9.
        assert_eq!(label(&lx, lx.read(1, None, &event(&[0xB0, 7, 5]))).0, "Preset F9");
        // Preset 2: it's F1 (and F9's too: the first wins), channel 2 is F2.
        let options = Options { variants: BTreeMap::from([("preset".to_string(), 1)]), one_port: false };
        let mixer = lx.with_options(options);
        assert_eq!(label(&mixer, mixer.read(1, None, &event(&[0xB0, 7, 5]))).0, "Preset F1");
        assert_eq!(label(&mixer, mixer.read(1, None, &event(&[0xB1, 7, 5]))).0, "Preset F2");
        assert!(mixer.read(1, None, &event(&[0xB0, 73, 5])).is_none());
        // Preset 5's buttons don't latch.
        let five = lx.with_options(Options { variants: BTreeMap::from([("preset".to_string(), 4)]), one_port: false });
        assert_eq!(five.read(1, None, &event(&[0xB0, 66, 0])).unwrap().1, Reading::Release);
    }

    #[test]
    fn one_port_reads_everything_on_port_1_preferring_the_page() {
        let lx = lx().with_options(Options { one_port: true, ..Options::default() });
        // Mixer F1 is CC 12 on channel 16; so is preset 3's B8, on another page.
        let mixer = lx.pages.iter().position(|page| page.id == "mixer");
        assert_eq!(label(&lx, lx.read(1, mixer, &event(&[0xBF, 12, 5]))).0, "Mixer F1");
        assert_eq!(label(&lx, lx.read(2, mixer, &event(&[0x9F, 60, 5]))).0, "P1");
    }

    #[test]
    fn shift_makes_a_button_its_twin() {
        let lx = lx();
        let mut reader = Reader::new(&lx);
        let mut read = |bytes: &[u8]| {
            let read = reader.read(&lx, 2, &event(bytes));
            label(&lx, read.control).0.to_string()
        };
        assert_eq!(read(&[0xBF, 106, 127]), "Play");
        assert_eq!(read(&[0xBF, 106, 0]), "Play");
        assert_eq!(read(&[0xBF, 108, 127]), "Shift");
        assert_eq!(read(&[0xBF, 106, 127]), "Click");
        // Shift let go first: the release is still the twin's.
        assert_eq!(read(&[0xBF, 108, 0]), "Shift");
        assert_eq!(read(&[0xBF, 106, 0]), "Click");
        assert_eq!(read(&[0xBF, 106, 127]), "Play");
        let play = lx.controls.iter().position(|control| control.label == "Play").unwrap();
        let click = lx.controls[play].twin.unwrap();
        assert!(lx.visible(play, None, false) && !lx.visible(click, None, false));
        assert!(!lx.visible(play, None, true) && lx.visible(click, None, true));
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

    #[test]
    fn feedback_is_the_controls_own_message() {
        let json = r#"{ "id": "x", "name": "x", "banks": [
            { "name": "k", "kind": "knob", "columns": 2, "channel": 2, "feedback": true,
              "controls": [{ "cc": 7 }, { "cc": 8, "feedback": false }] },
            { "name": "p", "kind": "pad", "columns": 1, "prefix": "P", "feedback": true, "controls": [{ "note": 36 }] }
        ] }"#;
        let surface = Surface::from_json(json).unwrap();
        assert_eq!(surface.feedback(0, Shown::Position(1.0), 127), Some([0xB1, 7, 127]));
        assert_eq!(surface.feedback(1, Shown::Position(1.0), 127), None);
        assert_eq!(surface.feedback(2, Shown::Lit(true), 5), Some([0x90, 36, 5]));
        assert_eq!(surface.feedback(2, Shown::Lit(false), 5), Some([0x90, 36, 0]));
        // A note-on with velocity 0 is "off": "on" is at least 1.
        assert_eq!(surface.feedback(2, Shown::Lit(true), 0), Some([0x90, 36, 1]));
    }

    #[test]
    fn specs_round_trip_and_labels_are_unique() {
        for (_, json) in SHIPPED {
            let surface = Surface::from_json(json).unwrap();
            let written = serde_json::to_string(&surface.spec).unwrap();
            assert_eq!(Surface::from_json(&written).unwrap().controls, surface.controls);
        }
        let twice = r#"{ "id": "x", "name": "x", "banks": [
            { "name": "a", "kind": "knob", "columns": 1, "prefix": "K", "controls": [{ "cc": 1 }] },
            { "name": "b", "kind": "knob", "columns": 1, "prefix": "K", "controls": [{ "cc": 2 }] }
        ] }"#;
        assert!(Surface::from_json(twice).is_err());
    }
}
