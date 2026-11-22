//! Editing a control surface (Penpot: "MIDI learn 4" and "5"; see "Editing a
//! surface" in `docs/design/midi-learn.md`): a working copy of its file, banks
//! added, set up, moved and deleted, and controls learned by moving them — a
//! bank's in turn, a single one, or a whole device's, grouped into banks as
//! they come. The window side is in `learn.rs` (`Learn::connect_editor`).

use live_keys_engine::MidiEvent;
use live_keys_surfaces::{BankSpec, ControlKind, ControlSpec, Message, Numbering, Pressure, Surface, SurfaceSpec};

/// Banks snap to this grid when dragged.
const GRID: i32 = 4;
/// The gap a new bank keeps from the last.
const GAP: u32 = 16;

/// What the editor is listening for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listening {
    None,
    /// Each control moved goes to the next of bank `bank`'s, from `next`.
    Bank { bank: usize, next: usize },
    /// The next control moved is control `control` (of the whole surface).
    Control { control: usize },
    /// Every new control moved is added, grouped into banks.
    Device,
}

/// What a bank's controls send, as its settings show it: the message type,
/// and the first number and step between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Cc,
    Note,
    PitchBend,
}

impl MessageType {
    pub const ALL: [Self; 3] = [Self::Cc, Self::Note, Self::PitchBend];

    fn of(message: Message) -> Option<Self> {
        match message {
            Message::Cc(_) => Some(Self::Cc),
            Message::Note(_) => Some(Self::Note),
            Message::PitchBend => Some(Self::PitchBend),
            Message::ChannelPressure => None,
        }
    }

    fn message(self, number: u8) -> Message {
        match self {
            Self::Cc => Message::Cc(number),
            Self::Note => Message::Note(number),
            Self::PitchBend => Message::PitchBend,
        }
    }
}

fn number_of(message: Message) -> Option<u8> {
    match message {
        Message::Cc(number) | Message::Note(number) => Some(number),
        _ => None,
    }
}

/// A bank's message settings, worked out from its controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BankMessages {
    pub kind: MessageType,
    pub first: u8,
    pub step: u8,
}

/// The surface being edited.
pub struct Editor {
    /// The MIDI device it's for (a `midi_device_name`).
    pub device: String,
    pub spec: SurfaceSpec,
    pub selected: Option<usize>,
    pub listening: Listening,
    /// The last message learned, so a control sending a stream (a knob
    /// turned) counts once.
    last: Option<(u8, u8, Message)>,
    /// The bank learning a device added to last.
    growing: Option<usize>,
}

impl Editor {
    /// Edits `surface` for `device`, or a new, empty surface for it.
    pub fn new(device: &str, surface: Option<&Surface>) -> Self {
        let spec = surface.map(|surface| surface.spec.clone()).unwrap_or_else(|| SurfaceSpec {
            id: id_for(device),
            name: device.split(':').next().unwrap_or(device).trim().to_string(),
            devices: vec![device.to_string()],
            ports: Vec::new(),
            claim_ports: Vec::new(),
            output_port: 1,
            driver: None,
            pages: Vec::new(),
            banks: Vec::new(),
            defaults: Vec::new(),
        });
        Self { device: device.to_string(), spec, selected: None, listening: Listening::None, last: None, growing: None }
    }

    /// The surface as laid out, or why it can't be.
    pub fn surface(&self) -> Result<Surface, String> {
        Surface::from_spec(self.spec.clone())
    }

    /// The right edge of the banks, where a new one goes.
    fn right_end(&self) -> u32 {
        self.surface().map_or(0, |surface| surface.width + if surface.banks.is_empty() { 0 } else { GAP })
    }

    /// Every message the surface's controls send, as (port, channel 0..15, message).
    fn messages(&self) -> Vec<(u8, u8, Message)> {
        self.surface()
            .map(|surface| {
                surface
                    .controls
                    .iter()
                    .filter(|control| control.twin_of.is_none())
                    .map(|control| (control.port, control.channel, control.message))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The lowest number from `from` where eight in a row of `kind` are
    /// free on channel 1.
    fn free_numbers(&self, kind: MessageType, from: u8) -> u8 {
        let used = self.messages();
        let free = |first: u8| (0..8u8).all(|offset| !used.contains(&(1, 0, kind.message(first.saturating_add(offset)))));
        (from..120).find(|&first| free(first)).unwrap_or(from)
    }

    /// Adds a bank of `kind` (or a single control) at the right end, selected.
    pub fn add_bank(&mut self, kind: ControlKind, single: bool) {
        let (name, prefix, rows, columns) = match (kind, single) {
            (ControlKind::Fader, false) => ("Faders", "F", 1, 8),
            (ControlKind::Knob, false) => ("Knobs", "K", 1, 8),
            (ControlKind::Pad, false) => ("Pads", "P", 2, 4),
            (ControlKind::Button, false) => ("Buttons", "B", 1, 8),
            (ControlKind::Fader, true) => ("Fader", "F", 1, 1),
            (ControlKind::Knob, true) => ("Knob", "K", 1, 1),
            (ControlKind::Pad, true) => ("Pad", "P", 1, 1),
            (ControlKind::Button, true) => ("Button", "B", 1, 1),
        };
        let message = if kind == ControlKind::Pad { MessageType::Note } else { MessageType::Cc };
        let first = self.free_numbers(message, if message == MessageType::Note { 36 } else { 20 });
        let same = self.spec.banks.iter().filter(|bank| bank.name.starts_with(name)).count();
        // Labels are the bank's prefix and number, and must be unique.
        let mut prefix = prefix.to_string();
        let mut count = 1;
        while self.spec.banks.iter().any(|bank| bank.prefix == prefix) {
            count += 1;
            prefix = format!("{}{count}.", &prefix[..1]);
        }
        let mut bank = BankSpec {
            name: if same == 0 { name.to_string() } else { format!("{name} {}", same + 1) },
            kind,
            x: self.right_end(),
            y: 0,
            rows,
            columns,
            numbering: Numbering::TopFirst,
            prefix,
            channel: 1,
            pressure: None,
            feedback: false,
            lit: None,
            port: 1,
            page: None,
            toggle: false,
            cell_width: None,
            cell_height: None,
            controls: Vec::new(),
            variants: Vec::new(),
        };
        fill(&mut bank, BankMessages { kind: message, first, step: 1 });
        self.spec.banks.push(bank);
        self.selected = Some(self.spec.banks.len() - 1);
    }

    pub fn delete_bank(&mut self, bank: usize) {
        if bank < self.spec.banks.len() {
            self.spec.banks.remove(bank);
            self.selected = None;
            self.listening = Listening::None;
        }
    }

    /// Moves bank `bank` to `x`, `y` (snapped to the grid).
    pub fn move_bank(&mut self, bank: usize, x: i32, y: i32) {
        let snap = |value: i32| (value.max(0) + GRID / 2) / GRID * GRID;
        if let Some(bank) = self.spec.banks.get_mut(bank) {
            bank.x = snap(x) as u32;
            bank.y = snap(y) as u32;
        }
    }

    /// Bank `bank`'s message settings (its first control's type and number,
    /// the step to the second's).
    pub fn bank_messages(&self, bank: usize) -> Option<BankMessages> {
        let controls = &self.spec.banks.get(bank)?.controls;
        let first = controls.first()?.message().ok()?;
        let second = controls.get(1).and_then(|control| control.message().ok());
        let step = match (number_of(first), second.and_then(number_of)) {
            (Some(a), Some(b)) if b > a => b - a,
            _ => 1,
        };
        Some(BankMessages { kind: MessageType::of(first)?, first: number_of(first).unwrap_or(0), step })
    }

    /// Changes bank `bank`'s shape; controls added continue its numbering.
    pub fn set_shape(&mut self, bank: usize, rows: u32, columns: u32) {
        let messages = self.bank_messages(bank);
        let Some(spec) = self.spec.banks.get_mut(bank) else { return };
        spec.rows = rows.clamp(1, 16);
        spec.columns = columns.clamp(1, 32);
        let count = (spec.rows * spec.columns) as usize;
        spec.controls.truncate(count);
        if let Some(messages) = messages {
            while spec.controls.len() < count {
                let index = spec.controls.len() as u32;
                spec.controls.push(ControlSpec::of(nth(messages, index)));
            }
        }
    }

    /// Gives bank `bank`'s controls `messages` in order.
    pub fn set_messages(&mut self, bank: usize, messages: BankMessages) {
        if let Some(spec) = self.spec.banks.get_mut(bank) {
            fill(spec, messages);
        }
    }

    /// Changes something else of bank `bank`.
    pub fn edit_bank(&mut self, bank: usize, change: impl FnOnce(&mut BankSpec)) {
        if let Some(spec) = self.spec.banks.get_mut(bank) {
            change(spec);
            if spec.kind != ControlKind::Pad {
                spec.pressure = None;
            }
        }
    }

    pub fn listen(&mut self, listening: Listening) {
        self.listening = listening;
        self.last = None;
        self.growing = None;
    }

    /// Skips a control while learning a bank.
    pub fn skip(&mut self) {
        if let Listening::Bank { bank, next } = self.listening {
            self.advance(bank, next);
        }
    }

    fn advance(&mut self, bank: usize, next: usize) {
        let count = self.spec.banks.get(bank).map_or(0, |bank| bank.controls.len());
        self.listening = if next + 1 < count { Listening::Bank { bank, next: next + 1 } } else { Listening::None };
    }

    /// A MIDI event from the device's port `port`: learned if the editor is
    /// listening. Returns whether it changed the surface.
    pub fn event(&mut self, port: u8, event: &MidiEvent) -> bool {
        let Some((channel, message)) = learnable(event) else { return false };
        if self.listening == Listening::None || self.last == Some((port, channel, message)) {
            return false;
        }
        self.last = Some((port, channel, message));
        match self.listening {
            Listening::None => false,
            Listening::Bank { bank, next } => {
                self.set_control(bank, next, port, channel, message);
                self.advance(bank, next);
                true
            }
            Listening::Control { control } => {
                let Some((bank, index)) = self.control_place(control) else { return false };
                self.set_control(bank, index, port, channel, message);
                self.listening = Listening::None;
                true
            }
            Listening::Device => self.learn_into_device(port, channel, message),
        }
    }

    /// Control `control` of the whole surface, as (bank, index in it).
    fn control_place(&self, control: usize) -> Option<(usize, usize)> {
        let mut left = control;
        for (bank, spec) in self.spec.banks.iter().enumerate() {
            if left < spec.controls.len() {
                return Some((bank, left));
            }
            left -= spec.controls.len();
        }
        None
    }

    /// Control `index` of bank `bank` sends `message` on `channel` (0..15)
    /// of port `port`. A control of the bank that sent it already gets this
    /// one's old message. (A bank has one port: the first control's.)
    fn set_control(&mut self, bank: usize, index: usize, port: u8, channel: u8, message: Message) {
        let Some(spec) = self.spec.banks.get_mut(bank) else { return };
        let bank_channel = spec.channel;
        let Some(old) = spec.controls.get(index).cloned() else { return };
        let sends = |control: &ControlSpec| {
            control.message().ok() == Some(message) && control.channel.unwrap_or(bank_channel) == channel + 1
        };
        if let Some(other) = spec.controls.iter().position(sends).filter(|&other| other != index) {
            let label = spec.controls[other].label.take();
            spec.controls[other] = ControlSpec { label, ..old.clone() };
        }
        let control = &mut spec.controls[index];
        *control = ControlSpec { label: old.label, ..ControlSpec::of(message) };
        // The first learned sets the bank's port and channel; others keep their own channel.
        if index == 0 {
            spec.channel = channel + 1;
            spec.port = port;
        } else if channel + 1 != spec.channel {
            control.channel = Some(channel + 1);
        }
    }

    /// Learning a device: `message` joins the bank the last one went into if
    /// it's the next number of the same type and channel, else starts a
    /// bank. Controls the surface has already are ignored.
    fn learn_into_device(&mut self, port: u8, channel: u8, message: Message) -> bool {
        if self.messages().contains(&(port, channel, message)) {
            return false;
        }
        if let Some(bank) = self.growing
            && let Some(spec) = self.spec.banks.get_mut(bank)
            && let Some(last) = spec.controls.last().and_then(|control| control.message().ok())
            && spec.channel == channel + 1
            && spec.port == port
            && MessageType::of(last) == MessageType::of(message)
            && number_of(last).zip(number_of(message)).is_some_and(|(a, b)| b == a.wrapping_add(1))
        {
            spec.columns += 1;
            spec.controls.push(ControlSpec::of(message));
            return true;
        }
        let kind = match message {
            Message::Note(_) => ControlKind::Pad,
            Message::PitchBend => ControlKind::Fader,
            _ => ControlKind::Knob,
        };
        self.add_bank(kind, true);
        let bank = self.spec.banks.len() - 1;
        let spec = &mut self.spec.banks[bank];
        spec.name = match kind {
            ControlKind::Pad => "Pads",
            ControlKind::Fader => "Wheel",
            _ => "Knobs",
        }
        .to_string();
        spec.channel = channel + 1;
        spec.port = port;
        spec.controls = vec![ControlSpec::of(message)];
        self.growing = Some(bank);
        self.selected = Some(bank);
        true
    }

    /// What the editor is doing, for the status line.
    pub fn status(&self) -> String {
        let name = |bank: usize, index: usize| {
            let spec = &self.spec.banks[bank];
            spec.controls
                .get(index)
                .and_then(|control| control.label.clone())
                .unwrap_or_else(|| format!("{}{}", spec.prefix, index + 1))
        };
        match self.listening {
            Listening::None => match self.surface() {
                Ok(_) => String::new(),
                Err(err) => format!("Can't be saved yet: {err}"),
            },
            Listening::Bank { bank, next } => format!(
                "Listening: move or press {} · {} of {}",
                name(bank, next),
                next + 1,
                self.spec.banks[bank].controls.len()
            ),
            Listening::Control { control } => match self.control_place(control) {
                Some((bank, index)) => format!("Listening: move or press {}", name(bank, index)),
                None => String::new(),
            },
            Listening::Device => "Listening: move each control in turn; new ones are added.".to_string(),
        }
    }
}

/// Bank control `index`'s message for `messages`.
fn nth(messages: BankMessages, index: u32) -> Message {
    let number = u32::from(messages.first) + index * u32::from(messages.step.max(1));
    messages.kind.message(number.min(127) as u8)
}

/// Gives every control of `bank` (as many as its shape holds) a message from
/// `messages`, keeping their labels.
fn fill(bank: &mut BankSpec, messages: BankMessages) {
    let count = (bank.rows * bank.columns) as usize;
    let labels: Vec<Option<String>> = bank.controls.iter().map(|control| control.label.clone()).collect();
    bank.controls = (0..count)
        .map(|index| {
            let mut control = ControlSpec::of(nth(messages, index as u32));
            control.label = labels.get(index).cloned().flatten();
            control
        })
        .collect();
    // Only one control can be pitch bend.
    if messages.kind == MessageType::PitchBend {
        bank.rows = 1;
        bank.columns = 1;
        bank.controls.truncate(1);
    }
}

/// The control message `event` is from, as (channel 0..15, message): CCs,
/// note-ons, pitch bend.
fn learnable(event: &MidiEvent) -> Option<(u8, Message)> {
    let bytes = event.as_slice();
    let status = *bytes.first()?;
    let data = |index: usize| bytes.get(index).copied().unwrap_or(0);
    let channel = status & 0x0F;
    match status & 0xF0 {
        0xB0 => Some((channel, Message::Cc(data(1)))),
        0x90 if data(2) > 0 => Some((channel, Message::Note(data(1)))),
        0xE0 => Some((channel, Message::PitchBend)),
        _ => None,
    }
}

/// A surface id for `device`: its name in lowercase, words joined by `-`.
fn id_for(device: &str) -> String {
    let name = device.split(':').next().unwrap_or(device);
    let words: Vec<String> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() { "surface".to_string() } else { words.join("-") }
}

/// Pressure options as the settings number them (0 none, 1 channel, 2 poly).
pub fn pressure_index(pressure: Option<Pressure>) -> i32 {
    match pressure {
        None => 0,
        Some(Pressure::Channel) => 1,
        Some(Pressure::Poly) => 2,
    }
}

pub fn pressure_of(index: i32) -> Option<Pressure> {
    match index {
        1 => Some(Pressure::Channel),
        2 => Some(Pressure::Poly),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(bytes: &[u8]) -> MidiEvent {
        MidiEvent::from_raw(0, bytes).unwrap()
    }

    #[test]
    fn learning_a_device_groups_consecutive_controls() {
        let mut editor = Editor::new("Thing:Thing MIDI 1", None);
        assert_eq!(editor.spec.id, "thing");
        editor.listen(Listening::Device);
        // A knob turned sends a stream: counted once.
        for value in [1, 2, 3] {
            editor.event(1, &event(&[0xB0, 70, value]));
        }
        editor.event(1, &event(&[0xB0, 71, 5]));
        editor.event(1, &event(&[0xB0, 72, 5]));
        // Pads on another channel start a bank of their own.
        editor.event(1, &event(&[0x99, 36, 100]));
        editor.event(1, &event(&[0x89, 36, 0]));
        editor.event(1, &event(&[0x99, 37, 100]));
        // Not consecutive: a new bank.
        editor.event(1, &event(&[0xB0, 10, 5]));
        // Already there: ignored.
        editor.event(1, &event(&[0xB0, 71, 9]));
        let banks: Vec<(ControlKind, u32, u8)> =
            editor.spec.banks.iter().map(|bank| (bank.kind, bank.columns, bank.channel)).collect();
        assert_eq!(banks, [(ControlKind::Knob, 3, 1), (ControlKind::Pad, 2, 10), (ControlKind::Knob, 1, 1)]);
        let surface = editor.surface().unwrap();
        assert!(surface.banks[1].x > surface.banks[0].x);
    }

    #[test]
    fn learning_a_bank_fills_it_in_turn() {
        let mut editor = Editor::new("Pads", None);
        editor.add_bank(ControlKind::Pad, false);
        editor.listen(Listening::Bank { bank: 0, next: 0 });
        editor.event(1, &event(&[0x99, 50, 1]));
        editor.skip();
        editor.event(1, &event(&[0x99, 52, 1]));
        let bank = &editor.spec.banks[0];
        assert_eq!(bank.channel, 10);
        assert_eq!(bank.controls[0].note, Some(50));
        assert_eq!(bank.controls[2].note, Some(52));
        assert_eq!(editor.listening, Listening::Bank { bank: 0, next: 3 });
        // The shape grows with its numbering.
        editor.set_messages(0, BankMessages { kind: MessageType::Cc, first: 20, step: 2 });
        editor.set_shape(0, 3, 4);
        assert_eq!(editor.spec.banks[0].controls[11].cc, Some(42));
    }
}
