//! An instrument's controls: named parameters (what a panel, a host or a preset
//! sees), each driving one SFZ controller or keyswitch. The instrument's SFZ is
//! written so that each controller, 0..=1, spans the control's range linearly;
//! the sound design stays in the SFZ.
//!
//! Also the instruments' state format, for presets and saved state: UTF-8
//! text, a `symbol = value` line per control, `#` comments. Unknown symbols are
//! ignored and missing ones take their default, so controls can be added.

use crate::synth::Synth;
use anyhow::{Context, Result};

/// What a control drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlTarget {
    /// An SFZ controller (`set_hdccN`, `*_onccN`).
    Cc(u16),
    /// A keyswitch per choice (`sw_last`).
    Keyswitch(&'static [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Control {
    /// Stable across versions: what presets and host-side settings refer to.
    pub symbol: &'static str,
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// Shown after a value ("dB", "%"); empty for none.
    pub unit: &'static str,
    /// For a choice: its options, as values 0, 1, … (`min` 0, `max` their count − 1).
    pub choices: &'static [&'static str],
    pub target: ControlTarget,
}

impl Control {
    /// `value` as 0..=1 across the control's range.
    pub fn normalized(&self, value: f32) -> f32 {
        if self.max > self.min { ((value - self.min) / (self.max - self.min)).clamp(0.0, 1.0) } else { 0.0 }
    }

    /// Applies `value` to the synth. Real-time safe.
    pub fn apply(&self, synth: &mut Synth, value: f32) {
        match self.target {
            ControlTarget::Cc(cc) => synth.set_cc(0, cc, self.normalized(value)),
            ControlTarget::Keyswitch(keys) => {
                let index = value.round().clamp(0.0, keys.len().saturating_sub(1) as f32) as usize;
                synth.set_keyswitch(keys.get(index).copied());
            }
        }
    }
}

/// The controls' values (by index) as state text.
pub fn write_state(controls: &[Control], value: impl Fn(usize) -> f32) -> String {
    let mut text = String::from("# live-keys instrument state\n");
    for (i, control) in controls.iter().enumerate() {
        text.push_str(&format!("{} = {}\n", control.symbol, value(i)));
    }
    text
}

/// Every control's value from state text, by index: as saved, else its default.
pub fn read_state(controls: &[Control], text: &str) -> Result<Vec<f32>> {
    let mut values: Vec<f32> = controls.iter().map(|c| c.default).collect();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (symbol, value) = line.split_once('=').with_context(|| format!("line {}: no '='", number + 1))?;
        let value: f32 = value.trim().parse().with_context(|| format!("line {}: not a number", number + 1))?;
        if let Some(i) = controls.iter().position(|c| c.symbol == symbol.trim()) {
            values[i] = value.clamp(controls[i].min, controls[i].max);
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROLS: [Control; 2] = [
        Control { symbol: "tone", name: "Tone", min: -12.0, max: 12.0, default: 0.0, unit: "dB", choices: &[], target: ControlTarget::Cc(23) },
        Control { symbol: "mode", name: "Mode", min: 0.0, max: 1.0, default: 0.0, unit: "", choices: &["A", "B"], target: ControlTarget::Keyswitch(&[12, 13]) },
    ];

    #[test]
    fn state_round_trip_with_defaults_and_unknowns() {
        let text = write_state(&CONTROLS, |i| [6.5, 1.0][i]);
        assert_eq!(read_state(&CONTROLS, &text).unwrap(), [6.5, 1.0]);
        assert_eq!(read_state(&CONTROLS, "# c\nold = 3\ntone = 40\n").unwrap(), [12.0, 0.0]);
        assert!(read_state(&CONTROLS, "tone 3").is_err());
        assert_eq!(CONTROLS[0].normalized(0.0), 0.5);
    }
}
