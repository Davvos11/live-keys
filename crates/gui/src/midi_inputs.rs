//! The Settings tab's MIDI inputs: each input's devices, as checklists of what's
//! plugged in plus what the input remembers.

use std::cell::RefCell;
use std::rc::Rc;

use live_keys_engine::midi_device_name;
use slint::{ModelRc, VecModel};

use crate::config::AppConfig;
use crate::{MidiDeviceRow, MidiInputRow, sync_model};

pub struct MidiInputsUi {
    pub rows: Rc<VecModel<MidiInputRow>>,
    /// Each row's device list, kept so it's refilled in place: replacing it
    /// would rebuild an open checklist under the pointer.
    devices: RefCell<Vec<Rc<VecModel<MidiDeviceRow>>>>,
    /// Device names of the MIDI ports present at the last [`MidiInputsUi::set_ports`].
    present: RefCell<Vec<String>>,
}

impl MidiInputsUi {
    pub fn new() -> Self {
        Self {
            rows: Rc::new(VecModel::default()),
            devices: RefCell::default(),
            present: RefCell::default(),
        }
    }

    /// Records which MIDI ports are present (as the engine lists them); refreshes
    /// if that changed.
    pub fn set_ports(&self, ports: &[String], config: &AppConfig) {
        let mut present: Vec<String> = Vec::new();
        for port in ports {
            let name = midi_device_name(port);
            if !present.iter().any(|known| known == name) {
                present.push(name.to_string());
            }
        }
        if *self.present.borrow() != present {
            *self.present.borrow_mut() = present;
            self.refresh(config);
        }
    }

    /// Input `index` was removed: its rows go, and the ones after it move up.
    pub fn remove(&self, index: usize, config: &AppConfig) {
        let mut devices = self.devices.borrow_mut();
        if index < devices.len() {
            devices.remove(index);
            self.rows.remove(index);
        }
        drop(devices);
        self.refresh(config);
    }

    /// Shows `config`'s inputs against the ports last present.
    pub fn refresh(&self, config: &AppConfig) {
        let present = self.present.borrow();
        let mut devices = self.devices.borrow_mut();
        devices.resize_with(config.midi_inputs.len(), Default::default);
        let mut rows = Vec::new();
        for (input, model) in config.midi_inputs.iter().zip(devices.iter()) {
            let available = |name: &str| present.iter().any(|known| known == name);
            let missing = input.devices.iter().filter(|name| !available(name));
            let device_rows: Vec<MidiDeviceRow> = present
                .iter()
                .chain(missing.clone())
                .map(|name| MidiDeviceRow {
                    name: name.into(),
                    checked: input.devices.contains(name),
                    available: available(name),
                })
                .collect();
            sync_model(model, device_rows);

            let mut summary: Vec<String> = input
                .devices
                .iter()
                .filter(|name| available(name))
                .cloned()
                .collect();
            summary.extend(missing.map(|name| format!("{name} (not connected)")));
            let summary = if summary.is_empty() {
                "No devices".to_string()
            } else {
                summary.join(", ")
            };
            rows.push(MidiInputRow {
                summary: summary.into(),
                devices: ModelRc::from(model.clone()),
            });
        }
        sync_model(&self.rows, rows);
    }
}

/// `mask` (bit `n` = input `n`) with input `removed` taken out: the inputs after
/// it move down a bit.
pub fn remove_input_bit(mask: u32, removed: usize) -> u32 {
    let below = mask & ((1 << removed) - 1);
    let above = mask.checked_shr(removed as u32 + 1).unwrap_or(0) << removed;
    below | above
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_an_input_shifts_the_later_ones_down() {
        assert_eq!(remove_input_bit(0b1011, 1), 0b101);
        assert_eq!(remove_input_bit(0b1011, 0), 0b101);
        assert_eq!(remove_input_bit(0b0001, 3), 0b0001);
        assert_eq!(remove_input_bit(u32::MAX, 31), u32::MAX >> 1);
    }
}
