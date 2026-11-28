//! Two plugin extensions about controllers: **remote controls** (the plugin's
//! own pages of up to 8 parameters it suggests for hardware knobs — the host
//! uses the first as the default panel or sliders of a plugin without a
//! profile) and **parameter indication** (the host tells the plugin which of its
//! parameters a controller is mapped to, which its GUI may show).

use std::collections::BTreeMap;
use std::ffi::{CString, c_char};

use clap_sys::ext::param_indication::clap_plugin_param_indication;
use clap_sys::ext::remote_controls::{CLAP_REMOTE_CONTROLS_COUNT, clap_plugin_remote_controls, clap_remote_controls_page};
use clap_sys::id::CLAP_INVALID_ID;
use clap_sys::plugin::clap_plugin;

/// One page of a plugin's remote controls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteControlsPage {
    pub section: String,
    pub name: String,
    /// Its parameters, in order (unassigned places left out).
    pub params: Vec<u32>,
}

/// What a parameter is mapped to, for the plugin's GUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamIndication {
    /// Short: the control's label ("K1").
    pub label: String,
    /// Longer: the device and the control ("Nektar Impact LX88+ K1").
    pub description: String,
}

/// Every page `ext` gives (main thread).
pub(crate) fn read_pages(plugin: *const clap_plugin, ext: &clap_plugin_remote_controls) -> Vec<RemoteControlsPage> {
    let (Some(count), Some(get)) = (ext.count, ext.get) else { return Vec::new() };
    let count = unsafe { count(plugin) };
    (0..count)
        .filter_map(|index| {
            let mut page: clap_remote_controls_page = unsafe { std::mem::zeroed() };
            if !unsafe { get(plugin, index, &mut page) } {
                return None;
            }
            Some(RemoteControlsPage {
                section: c_text(&page.section_name),
                name: c_text(&page.page_name),
                params: page.param_ids[..CLAP_REMOTE_CONTROLS_COUNT]
                    .iter()
                    .copied()
                    .filter(|&id| id != CLAP_INVALID_ID)
                    .collect(),
            })
        })
        .collect()
}

/// Tells the plugin what changed between `old` and `new` (main thread): new and
/// changed mappings are set, ones gone are cleared.
pub(crate) fn indicate(
    plugin: *const clap_plugin,
    ext: &clap_plugin_param_indication,
    old: &BTreeMap<u32, ParamIndication>,
    new: &BTreeMap<u32, ParamIndication>,
) {
    let Some(set_mapping) = ext.set_mapping else { return };
    for (&param, indication) in new {
        if old.get(&param) == Some(indication) {
            continue;
        }
        let label = c_string(&indication.label);
        let description = c_string(&indication.description);
        unsafe { set_mapping(plugin, param, true, std::ptr::null(), label.as_ptr(), description.as_ptr()) };
    }
    for &param in old.keys().filter(|param| !new.contains_key(param)) {
        unsafe { set_mapping(plugin, param, false, std::ptr::null(), std::ptr::null(), std::ptr::null()) };
    }
}

/// A fixed-size C string field as text (up to its first NUL).
fn c_text(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `text` for C, inner NULs dropped.
fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', "")).unwrap_or_default()
}
