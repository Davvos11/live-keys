use anyhow::{Context, Result};

/// Lists MIDI input port names.
pub fn list_midi_ports() -> Result<Vec<String>> {
    let midi_input = midir::MidiInput::new("live-keys")?;
    midi_input
        .ports()
        .iter()
        .map(|port| midi_input.port_name(port).map_err(anyhow::Error::from))
        .collect()
}

/// Selects a MIDI input port by case-insensitive substring match on its name, or the
/// first available port if `name_filter` is `None`.
pub fn find_midi_port<'a>(
    midi_input: &midir::MidiInput,
    ports: &'a [midir::MidiInputPort],
    name_filter: Option<&str>,
) -> Result<&'a midir::MidiInputPort> {
    match name_filter {
        None => ports
            .first()
            .context("no MIDI input devices found; connect a MIDI keyboard and try again"),
        Some(filter) => {
            let filter = filter.to_lowercase();
            ports
                .iter()
                .find(|port| {
                    midi_input
                        .port_name(port)
                        .is_ok_and(|name| name.to_lowercase().contains(&filter))
                })
                .with_context(|| format!("no MIDI input port matching '{filter}' found"))
        }
    }
}
