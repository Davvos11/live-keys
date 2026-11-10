//! Writes `ui/mock_data.slint`: the UI state of a running session, as Slint
//! literals, which `AppState` and `ParamsWindow` fall back to in the editor's
//! preview (`Platform.uses-mock-data`). Run the app with `LIVE_KEYS_MOCK_DATA=1`
//! (or a path instead of `1`), set up what the preview should show, and quit: the
//! file is written on exit.

use std::fmt::Write;
use std::path::PathBuf;

use slint::{ComponentHandle, Model, ModelRc, SharedString};

use crate::{
    AppState, AppWindow, EffectRow, MidiDeviceRow, MidiInputRow, ParamRow, PickerRow, PresetRow,
    SceneRow, SceneSlotChoice, SlotPresetRow, SlotState, KnobRow,
};

/// Where to write the mock data, if asked to.
pub fn requested_path() -> Option<PathBuf> {
    let value = std::env::var_os("LIVE_KEYS_MOCK_DATA")?;
    Some(if value == "1" {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/mock_data.slint"))
    } else {
        PathBuf::from(value)
    })
}

/// A value as a Slint literal.
trait ToSlint {
    fn write_slint(&self, out: &mut String, indent: usize);
}

impl ToSlint for SharedString {
    fn write_slint(&self, out: &mut String, _indent: usize) {
        out.push('"');
        for c in self.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
}

impl ToSlint for i32 {
    fn write_slint(&self, out: &mut String, _indent: usize) {
        let _ = write!(out, "{self}");
    }
}

impl ToSlint for f32 {
    fn write_slint(&self, out: &mut String, _indent: usize) {
        // Display never uses exponents; Slint has no literal for NaN/infinity.
        let value = if self.is_finite() { *self } else { 0.0 };
        let _ = write!(out, "{value}");
    }
}

impl ToSlint for bool {
    fn write_slint(&self, out: &mut String, _indent: usize) {
        out.push_str(if *self { "true" } else { "false" });
    }
}

impl<T: ToSlint + Clone + 'static> ToSlint for ModelRc<T> {
    fn write_slint(&self, out: &mut String, indent: usize) {
        if self.row_count() == 0 {
            out.push_str("[]");
            return;
        }
        out.push_str("[\n");
        for row in self.iter() {
            pad(out, indent + 1);
            row.write_slint(out, indent + 1);
            out.push_str(",\n");
        }
        pad(out, indent);
        out.push(']');
    }
}

fn pad(out: &mut String, indent: usize) {
    out.push_str(&"    ".repeat(indent));
}

/// Implements [`ToSlint`] for a struct the UI defines, as an anonymous struct
/// literal of the listed fields (Rust's snake_case names become Slint's
/// kebab-case). A field missing from the list is left at its default.
macro_rules! slint_struct {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl ToSlint for $ty {
            fn write_slint(&self, out: &mut String, indent: usize) {
                out.push_str("{\n");
                $(
                    pad(out, indent + 1);
                    out.push_str(&stringify!($field).replace('_', "-"));
                    out.push_str(": ");
                    self.$field.write_slint(out, indent + 1);
                    out.push_str(",\n");
                )*
                pad(out, indent);
                out.push('}');
            }
        }
    };
}

slint_struct!(PickerRow { id, name, format, pinned, after_pinned });
slint_struct!(ParamRow {
    param_id, name, kind, min_value, max_value, steps, logarithmic, value, text, choices,
    choice_values, choice_index,
});
slint_struct!(PresetRow { name, user, pinned, after_pinned, current });
slint_struct!(EffectRow {
    key, name, gui_supported, gui_open, params_open, graph, preset_name, preset_user, preset_changed,
});
slint_struct!(SlotPresetRow { name, folder, count, current });
slint_struct!(MidiDeviceRow { name, checked, available });
slint_struct!(MidiInputRow { summary, devices });
slint_struct!(SceneRow { name, current, modes });
slint_struct!(SceneSlotChoice { mode, can_restore });
slint_struct!(KnobRow {
    key, param_id, name, position, default_position, min_value, max_value, logarithmic, text,
});
slint_struct!(SlotState {
    artwork, muted, closed_name, knobs, instrument, status, midi_effects, audio_effects, midi_inputs, midi_channel, note_low,
    note_high, velocity_low, velocity_high, volume_db, slot_preset_name, slot_preset_changed,
    slot_preset_folder, slot_preset_rows,
});

/// One `out property` of the `MockData` global.
fn property(out: &mut String, ty: &str, name: &str, value: &dyn ToSlint) {
    let _ = write!(out, "    out property <{ty}> {name}: ");
    value.write_slint(out, 1);
    out.push_str(";\n");
}

/// The mock data file for the window's current state; `params_window` is a
/// plugin's name and parameter rows, for previewing `ParamsWindow`.
pub fn generate(app: &AppWindow, params_window: (SharedString, ModelRc<ParamRow>)) -> String {
    let state = app.global::<AppState>();
    let mut out = String::from(
        "// Generated from a running session by `LIVE_KEYS_MOCK_DATA=1 cargo run -p live-keys-gui`\n\
         // (see `src/mock_data.rs`), for the editor's preview: `AppState` and `ParamsWindow` fall\n\
         // back to these when `Platform.uses-mock-data`. Regenerate rather than edit by hand.\n\n\
         import { PickerRow, ParamRow, PresetRow, SlotPresetRow, EffectRow, MidiDeviceRow, MidiInputRow, \
         SceneRow, SceneSlotChoice, SlotState, KnobRow } from \"types.slint\";\n\n\
         export global MockData {\n",
    );
    property(&mut out, "[string]", "audio-devices", &state.get_audio_devices());
    property(&mut out, "int", "selected-audio-index", &state.get_selected_audio_index());
    property(&mut out, "[MidiInputRow]", "midi-inputs", &state.get_midi_inputs());
    property(&mut out, "[SceneRow]", "scene-rows", &state.get_scene_rows());
    property(&mut out, "int", "scene-count", &state.get_scene_count());
    property(&mut out, "string", "current-scene", &state.get_current_scene());
    property(&mut out, "bool", "current-scene-changed", &state.get_current_scene_changed());
    property(&mut out, "[SceneSlotChoice]", "scene-form-slots", &state.get_scene_form_slots());
    property(&mut out, "string", "scene-status", &state.get_scene_status());
    property(&mut out, "string", "scene-target-name", &state.get_scene_target_name());
    property(&mut out, "[string]", "default-plugin-folders", &state.get_default_plugin_folders());
    property(&mut out, "[string]", "extra-plugin-folders", &state.get_extra_plugin_folders());
    property(&mut out, "[string]", "discovered-plugins", &state.get_discovered_plugins());
    property(&mut out, "[PickerRow]", "picker-rows", &state.get_picker_rows());
    property(&mut out, "[SlotState]", "slots", &state.get_slots());
    property(&mut out, "int", "preset-browser-key", &state.get_preset_browser_key());
    property(&mut out, "[PresetRow]", "preset-rows", &state.get_preset_rows());
    property(&mut out, "int", "preset-count", &state.get_preset_count());
    property(&mut out, "string", "preset-query", &state.get_preset_query());
    property(&mut out, "string", "params-window-name", &params_window.0);
    property(&mut out, "[ParamRow]", "params-window-params", &params_window.1);
    out.push_str("}\n");
    out
}
