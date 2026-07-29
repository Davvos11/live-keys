use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};

/// Lists logical audio output device names (see [`is_logical_output_device`]).
pub fn list_logical_output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    Ok(host
        .output_devices()?
        .filter(is_logical_output_device)
        .map(|device| device.to_string())
        .collect())
}

/// Selects an audio output device by case-insensitive substring match on its name, or
/// the system default output device if `name_filter` is `None`. Only considers
/// logical devices (see [`is_logical_output_device`]).
pub fn find_output_device(host: &cpal::Host, name_filter: Option<&str>) -> Result<cpal::Device> {
    match name_filter {
        None => host
            .default_output_device()
            .context("no default audio output device found"),
        Some(filter) => {
            let filter = filter.to_lowercase();
            host.output_devices()?
                .filter(is_logical_output_device)
                .find(|device| device.to_string().to_lowercase().contains(&filter))
                .with_context(|| format!("no audio output device matching '{filter}' found"))
        }
    }
}

/// ALSA-registered top-level output targets that aren't tied to a specific sound
/// card (so they have no `CARD=` in their driver id) but are still genuine, commonly
/// selected playback destinations — as opposed to generic plugin *types* like a rate
/// converter or channel mixer, which are building blocks for an `asoundrc` chain and
/// never meant to be picked directly.
const CARDLESS_OUTPUT_ALLOWLIST: &[&str] = &["default", "pulse", "pipewire", "jack"];

/// Filters out entries that aren't real, directly selectable audio output
/// destinations:
/// - ALSA's raw numeric hardware nodes (`hw:CARD=...`, `plughw:CARD=...`) — a
///   named/logical alternative (`sysdefault:CARD=...`, `front:CARD=...`, ...) always
///   exists alongside them, so keeping both just duplicates every sound card.
/// - ALSA's generic plugin *type* registrations with no card association at all
///   (`null`, `samplerate`, `speexrate`, `oss`, `upmix`, ...) — these are building
///   blocks for constructing a custom PCM chain, not something to play audio out of
///   on their own, and show up in the hint list purely because every installed ALSA
///   plugin advertises itself there regardless of whether it's a usable endpoint.
///
/// A device is kept if its driver id either names an actual sound card (`CARD=`) or
/// is one of the small set of card-less names that genuinely do route to real
/// output (see [`CARDLESS_OUTPUT_ALLOWLIST`]). On backends where a device's driver
/// name is unavailable (i.e. not ALSA), the device is kept.
pub fn is_logical_output_device(device: &cpal::Device) -> bool {
    let Ok(description) = device.description() else {
        return true;
    };
    match description.driver() {
        Some(driver) => {
            driver.contains("CARD=") || CARDLESS_OUTPUT_ALLOWLIST.contains(&driver)
        }
        None => true,
    }
}
