//! Drivers: code for devices that need more than their controls' own messages —
//! a handshake that puts them in a mode for a host, a goodbye that gives them
//! back, a display, lights that aren't a control's own message. A surface
//! names its driver (`"driver"` in its file); the mapping thread runs it,
//! sending what it returns to the surface's output port.
//!
//! Drivers are pure (they turn what happens into messages), so they can be
//! tested without the hardware. Adding one: implement [`Driver`] and add it to
//! [`open`] and [`DRIVERS`].

use crate::{Message, Shown, Surface};

/// What a driver does for a device, as messages (each a complete MIDI
/// message: a SysEx or a channel message) for the surface's output port.
pub trait Driver: Send {
    /// The device is there and shown (or shown again after being plugged in):
    /// set it up.
    fn connect(&mut self, surface: &Surface) -> Vec<Vec<u8>>;

    /// The device is no longer shown, its driver was switched off, or the app
    /// is closing: give it back as it was.
    fn disconnect(&mut self, surface: &Surface) -> Vec<Vec<u8>>;

    /// Which of the surface's controls drive something now, per control:
    /// `None` if it doesn't, else the colour pair its mapping chose (a code
    /// of [`Driver::palette`]; `None` for a control without one).
    fn mapped(&mut self, _surface: &Surface, _mapped: &[Option<Option<u8>>]) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// The colour pairs control `control`'s light can be given, if it can.
    fn palette(&self, _surface: &Surface, _control: usize) -> Option<Palette> {
        None
    }

    /// Control `control`, waiting to pick its target up, was moved: the
    /// target is above it (`up`: move it up) or below.
    fn pick_up(&mut self, _surface: &Surface, _control: usize, _up: bool) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// How control `control` shows `shown`, if not as its own message
    /// ([`Surface::feedback`]).
    fn feedback(&self, _surface: &Surface, _control: usize, _shown: Shown) -> Option<Vec<Vec<u8>>> {
        None
    }
}

/// A colour a light can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Off,
    Yellow,
    Red,
    Green,
    Orange,
}

impl Colour {
    pub fn name(self) -> &'static str {
        match self {
            Colour::Off => "Off",
            Colour::Yellow => "Yellow",
            Colour::Red => "Red",
            Colour::Green => "Green",
            Colour::Orange => "Orange",
        }
    }

    /// As drawn (`0xRRGGBB`); `None` for off.
    pub fn rgb(self) -> Option<u32> {
        match self {
            Colour::Off => None,
            Colour::Yellow => Some(0xF2_D0_24),
            Colour::Red => Some(0xE5_3B_2E),
            Colour::Green => Some(0x3C_C8_4B),
            Colour::Orange => Some(0xF5_8A_1F),
        }
    }
}

/// A light's two colours, not lit and lit, as the device numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourPair {
    pub code: u8,
    pub unlit: Colour,
    pub lit: Colour,
}

impl ColourPair {
    /// "Orange / green".
    pub fn name(&self) -> String {
        format!("{} / {}", self.unlit.name(), self.lit.name().to_lowercase())
    }
}

/// The colour pairs a control's light can be given, and the one a mapped
/// control gets unless its mapping chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub pairs: &'static [ColourPair],
    pub default: u8,
}

/// The palette of control `control` of `surface`, whose driver is `driver`.
pub fn palette(driver: &str, surface: &Surface, control: usize) -> Option<Palette> {
    open(driver)?.palette(surface, control)
}

/// The drivers there are: id (as surfaces name them) and what the panel's
/// switch for it says.
pub const DRIVERS: &[(&str, &str)] = &[(NEKTAR_LX_PLUS, "DAW mode")];

const NEKTAR_LX_PLUS: &str = "nektar-impact-lx-plus";

/// A new driver by id.
pub fn open(id: &str) -> Option<Box<dyn Driver>> {
    match id {
        NEKTAR_LX_PLUS => Some(Box::new(NektarLxPlus::default())),
        _ => None,
    }
}

/// What the panel's switch for driver `id` says.
pub fn switch_label(id: &str) -> Option<&'static str> {
    DRIVERS.iter().find(|(known, _)| *known == id).map(|(_, label)| *label)
}

/// Nektar Impact LX+ keyboards (LX25+…LX88+) in DAW mode: their protocol is in
/// `docs/design/nektar-impact.md` (§9 as tested on an LX88+).
#[derive(Default)]
pub struct NektarLxPlus {
    /// The pads' colour codes last sent.
    pads: Option<[u8; 8]>,
}

/// The pads' notes on channel 16, port 2, in the order the colour message
/// numbers them (1–8: bottom row left to right, then the top row).
const PAD_NOTES: [u8; 8] = [60, 62, 64, 65, 67, 69, 71, 72];

/// A pad colour code: unlit colour, lit colour.
const OFF_YELLOW: u8 = 0;
const ORANGE_GREEN: u8 = 10;

/// The pads' colour pairs (Logic's `PAD_COLOR`, §9 of the notes).
const PAD_COLOURS: [ColourPair; 16] = {
    use Colour::*;
    const fn pair(code: u8, unlit: Colour, lit: Colour) -> ColourPair {
        ColourPair { code, unlit, lit }
    }
    [
        pair(0, Off, Yellow),
        pair(1, Red, Yellow),
        pair(2, Green, Yellow),
        pair(3, Off, Green),
        pair(4, Red, Green),
        pair(5, Yellow, Green),
        pair(6, Off, Red),
        pair(7, Green, Red),
        pair(8, Yellow, Red),
        pair(9, Orange, Yellow),
        pair(10, Orange, Green),
        pair(11, Orange, Red),
        pair(12, Off, Orange),
        pair(13, Red, Orange),
        pair(14, Green, Orange),
        pair(15, Yellow, Orange),
    ]
};

/// The Page button's CC (two LEDs in one: A blue, B white).
const PAGE: u8 = 119;
const CLIPS: u8 = 117;
const SCENES: u8 = 118;

impl NektarLxPlus {
    /// A Nektar SysEx: the header, `payload` (command, target, index, element,
    /// length, data), its checksum, the end.
    pub fn sysex(payload: &[u8]) -> Vec<u8> {
        let mut message = vec![0xF0, 0x00, 0x01, 0x77, 0x7F];
        message.extend_from_slice(payload);
        message.push(checksum(payload));
        message.push(0xF7);
        message
    }

    /// USB port mode: 2 sends the DAW controls on port 2, 0 gives the
    /// keyboard back.
    fn port_mode(mode: u8) -> Vec<u8> {
        Self::sysex(&[0x01, 0x05, 0x00, 0x01, 0x0F, 0x01, mode])
    }

    /// The pads as the host's (notes on port 2, lit from outside), or back.
    fn pads_active(active: bool) -> Vec<u8> {
        if active {
            Self::sysex(&[0x01, 0x09, 0x04, 0x01, 0x00, 0x01, 0x03])
        } else {
            Self::sysex(&[0x01, 0x09, 0x00, 0x00, 0x00, 0x01, 0x00])
        }
    }

    fn pad_colour(pad: u8, code: u8) -> Vec<u8> {
        Self::sysex(&[0x01, 0x06, 0x00, 0x00, pad, 0x01, code])
    }

    /// The display's pick-up hint: 2 "UP", 3 "dn".
    fn display(value: u8) -> Vec<u8> {
        Self::sysex(&[0x01, 0x06, 0x00, 0x01, 0x01, 0x01, value])
    }

    /// Which pad (0–7) control `control` is, if it's one.
    fn pad_of(surface: &Surface, control: usize) -> Option<usize> {
        let control = surface.controls.get(control)?;
        let Message::Note(note) = control.message else { return None };
        (control.channel == 15 && control.twin_of.is_none()).then_some(())?;
        PAD_NOTES.iter().position(|&pad| pad == note)
    }
}

/// Nektar's checksum: the 7-bit two's complement of the payload's sum.
pub fn checksum(payload: &[u8]) -> u8 {
    let sum: u32 = payload.iter().map(|&byte| u32::from(byte)).sum();
    ((128 - sum % 128) % 128) as u8
}

impl Driver for NektarLxPlus {
    fn connect(&mut self, _surface: &Surface) -> Vec<Vec<u8>> {
        self.pads = None;
        vec![
            Self::port_mode(2),
            Self::pads_active(true),
            vec![0xBF, CLIPS, 0],
            vec![0xBF, SCENES, 0],
        ]
    }

    fn disconnect(&mut self, _surface: &Surface) -> Vec<Vec<u8>> {
        self.pads = None;
        let mut messages: Vec<Vec<u8>> = (1..=8).map(|pad| Self::pad_colour(pad, OFF_YELLOW)).collect();
        messages.extend(PAD_NOTES.iter().map(|&note| vec![0x9F, note, 0]));
        messages.extend([
            vec![0xBF, CLIPS, 0],
            vec![0xBF, SCENES, 0],
            vec![0xBF, PAGE, 0],
            vec![0xBF, PAGE, 16],
            Self::pads_active(false),
            Self::port_mode(0),
        ]);
        messages
    }

    /// Mapped pads get their mapping's colours (orange, green while lit,
    /// unless chosen); the others are dark.
    fn mapped(&mut self, surface: &Surface, mapped: &[Option<Option<u8>>]) -> Vec<Vec<u8>> {
        let mut codes = [OFF_YELLOW; 8];
        for (control, colour) in mapped.iter().enumerate() {
            if let (Some(colour), Some(pad)) = (colour, Self::pad_of(surface, control)) {
                codes[pad] = colour.filter(|&code| usize::from(code) < PAD_COLOURS.len()).unwrap_or(ORANGE_GREEN);
            }
        }
        if self.pads == Some(codes) {
            return Vec::new();
        }
        let before = self.pads.replace(codes);
        (0..8)
            .filter(|&pad| before.is_none_or(|before| before[pad] != codes[pad]))
            .map(|pad| Self::pad_colour(pad as u8 + 1, codes[pad]))
            .collect()
    }

    fn palette(&self, surface: &Surface, control: usize) -> Option<Palette> {
        Self::pad_of(surface, control).map(|_| Palette { pairs: &PAD_COLOURS, default: ORANGE_GREEN })
    }

    fn pick_up(&mut self, _surface: &Surface, _control: usize, up: bool) -> Vec<Vec<u8>> {
        vec![Self::display(if up { 2 } else { 3 })]
    }

    /// The Page button: white (its B LED) when lit, blue (A) when not.
    fn feedback(&self, surface: &Surface, control: usize, shown: Shown) -> Option<Vec<Vec<u8>>> {
        let found = surface.controls.get(control)?;
        if found.message != Message::Cc(PAGE) || found.channel != 15 || found.twin_of.is_some() {
            return None;
        }
        let on = match shown {
            Shown::Lit(on) => on,
            Shown::Position(position) => position > 0.0,
        };
        Some(vec![vec![0xBF, PAGE, u8::from(!on)], vec![0xBF, PAGE, 16 + u8::from(on)]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every message Nektar's own scripts send with a constant checksum,
    /// and what we send.
    #[test]
    fn checksums_match_nektars() {
        let check = |payload: &[u8], expected: u8| assert_eq!(checksum(payload), expected, "{payload:02X?}");
        check(&[0x01, 0x05, 0x00, 0x01, 0x0F, 0x01, 0x02], 0x67);
        check(&[0x01, 0x05, 0x00, 0x01, 0x0F, 0x01, 0x00], 0x69);
        check(&[0x01, 0x06, 0x00, 0x01, 0x01, 0x01, 0x03], 0x73);
        check(&[0x01, 0x09, 0x00, 0x00, 0x00, 0x01, 0x00], 0x75);
        check(&[0x01, 0x09, 0x04, 0x01, 0x00, 0x01, 0x03], 0x6D);
        check(&[0x01, 0x0B, 0x00, 0x00, 0x01, 0x02, 0x45, 0x04], 0x28);
        assert_eq!(
            NektarLxPlus::port_mode(2),
            [0xF0, 0x00, 0x01, 0x77, 0x7F, 0x01, 0x05, 0x00, 0x01, 0x0F, 0x01, 0x02, 0x67, 0xF7]
        );
    }

    #[test]
    fn pads_are_coloured_when_their_mapping_changes() {
        let surface = Surface::from_json(crate::SHIPPED[0].1).unwrap();
        let mut driver = NektarLxPlus::default();
        driver.connect(&surface);
        let pad = |note: u8| surface.controls.iter().position(|c| c.message == Message::Note(note) && c.channel == 15).unwrap();
        let mut mapped = vec![None; surface.controls.len()];
        // First: all eight.
        assert_eq!(driver.mapped(&surface, &mapped).len(), 8);
        assert!(driver.mapped(&surface, &mapped).is_empty());
        // P3 (note 64) mapped, no colour chosen: only its colour is sent, the default.
        mapped[pad(64)] = Some(None);
        assert_eq!(driver.mapped(&surface, &mapped), [NektarLxPlus::pad_colour(3, ORANGE_GREEN)]);
        // Its mapping chooses off/red (6), P1 (note 60) gets red/green (4).
        mapped[pad(64)] = Some(Some(6));
        mapped[pad(60)] = Some(Some(4));
        assert_eq!(driver.mapped(&surface, &mapped), [NektarLxPlus::pad_colour(1, 4), NektarLxPlus::pad_colour(3, 6)]);
        assert!(driver.mapped(&surface, &mapped).is_empty());
        // Unmapped again: dark.
        mapped[pad(60)] = None;
        assert_eq!(driver.mapped(&surface, &mapped), [NektarLxPlus::pad_colour(1, OFF_YELLOW)]);
    }
}
