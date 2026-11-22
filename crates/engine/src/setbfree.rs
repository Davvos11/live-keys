//! setBfree's control messages: how the engine reads and sets the organ's
//! settings, which it has no control ports for (see "setBfree's protocol" in
//! `docs/design/plugins.md`).
//!
//! Every setting crosses setBfree's `control` input and `notify` output as an
//! `atom:Object` of type `sb3:controlmsg` with an `sb3:controlkey` string (the
//! function name, "upper.drawbar16") and an `sb3:controlval` int (0..=127). The
//! plugin reports nothing until it gets an `sb3:uiinit` object, which makes it
//! dump every function set so far; after that it reports what changes inside it
//! (keyboard CCs, program changes, state restores: `special.reinit`, then a
//! dump), but not what the host sent. All of this is control-thread work: the
//! atoms travel over the plugin's GUI channel like its own GUI's.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex};

use live_keys_core::Producer;
use live_keys_lv2_host::{PORT_EVENT_BYTES, PortEvent, livi};

/// setBfree's LV2 URI.
pub const SETBFREE_URI: &str = "http://gareus.org/oss/lv2/b_synth";
const SB3: &str = "http://gareus.org/oss/lv2/b_synth#";
const ATOM: &str = "http://lv2plug.in/ns/ext/atom#";
/// The function setBfree reports before re-sending everything after a state
/// restore: "forget what you knew".
const REINIT: &str = "special.reinit";
const SPEED_PRESET: &str = "rotary.speed-preset";
const SPEED_SELECT: &str = "rotary.speed-select";

/// The `rotary.speed-select` setBfree derives from a `rotary.speed-preset`
/// value: the preset (`v / 43`: slow, stop, fast) picks horn and drum speeds
/// (both slow, both stopped, both fast: 4, 0, 8 as `horn·3 + drum`), reported
/// as `ceil(n · 15.875)` (b_whirl/whirl.c).
fn speed_select(preset: u8) -> u8 {
    let n: f32 = match preset / 43 {
        0 => 4.0,
        1 => 0.0,
        _ => 8.0,
    };
    (n * 15.875).ceil() as u8
}

/// The URIDs the messages use, from the instance's own URID map.
#[derive(Clone, Copy)]
struct Urids {
    object: u32,
    string: u32,
    int: u32,
    controlmsg: u32,
    controlkey: u32,
    controlval: u32,
    uiinit: u32,
}

impl Urids {
    fn new(features: &livi::Features) -> Self {
        let map = |base: &str, name: &str| {
            let uri = CString::new(format!("{base}{name}")).expect("URIs have no NUL");
            features.urid(&uri)
        };
        Self {
            object: map(ATOM, "Object"),
            string: map(ATOM, "String"),
            int: map(ATOM, "Int"),
            controlmsg: map(SB3, "controlmsg"),
            controlkey: map(SB3, "controlkey"),
            controlval: map(SB3, "controlval"),
            uiinit: map(SB3, "uiinit"),
        }
    }
}

/// One setBfree instance's side of the protocol: its port, its URIDs and the
/// last known value of every function.
pub(crate) struct Setbfree {
    urids: Urids,
    /// The `control` input's port index.
    control_port: u32,
    /// The `notify` output's port index.
    notify_port: u32,
    values: HashMap<String, u8>,
    /// The mapping thread's way in (see [`SetbfreeSink`]).
    sink: Option<SetbfreeSink>,
}

/// Sends control messages to one setBfree instance from the mapping thread,
/// through the plugin's second atom channel
/// ([`Lv2Plugin::take_control_channel`](live_keys_lv2_host::Lv2Plugin::take_control_channel)).
#[derive(Clone)]
pub struct SetbfreeSink {
    urids: Urids,
    control_port: u32,
    /// Only the mapping thread locks it.
    producer: Arc<Mutex<Producer<PortEvent>>>,
}

impl SetbfreeSink {
    /// Queues `function` = `value` for the plugin's next block; `false` if
    /// the queue is full.
    pub(crate) fn send(&self, function: &str, value: u8) -> bool {
        let Some(event) = control_message(self.urids, self.control_port, function, value.min(127)) else {
            return false;
        };
        self.producer.lock().is_ok_and(|mut producer| producer.push(event).is_ok())
    }
}

impl Setbfree {
    /// For a loaded plugin, if it's setBfree.
    /// `control` is the plugin's second atom channel, for the mapping thread.
    pub(crate) fn new(
        plugin: &livi::Plugin,
        features: &livi::Features,
        control: Option<Producer<PortEvent>>,
    ) -> Option<Self> {
        if plugin.uri() != SETBFREE_URI {
            return None;
        }
        let port = |symbol: &str| Some(plugin.ports().find(|port| port.symbol == symbol)?.index.0 as u32);
        let urids = Urids::new(features);
        let control_port = port("control")?;
        Some(Self {
            urids,
            control_port,
            notify_port: port("notify")?,
            values: HashMap::new(),
            sink: control.map(|producer| SetbfreeSink { urids, control_port, producer: Arc::new(Mutex::new(producer)) }),
        })
    }

    pub(crate) fn sink(&self) -> Option<SetbfreeSink> {
        self.sink.clone()
    }

    /// The empty `sb3:uiinit` object that starts reporting (and a dump).
    pub(crate) fn uiinit(&self) -> PortEvent {
        let mut writer = ObjectWriter::new(self.control_port, self.urids.object, self.urids.uiinit);
        writer.finish()
    }

    /// The control message setting `function` to `value`, recorded as sent:
    /// for the plugin's `control` input, and the messages for its GUI, as the
    /// plugin reports them on `notify` (it doesn't echo what the host sets).
    /// Usually the same message; `rotary.speed-preset`, which setBfree's GUI
    /// ignores, also gets the `rotary.speed-select` it derives (the GUI's
    /// horn and drum levers).
    pub(crate) fn set(&mut self, function: &str, value: u8) -> Option<(PortEvent, Vec<PortEvent>)> {
        let to_plugin = self.message(self.control_port, function, value.min(127))?;
        Some((to_plugin, self.record(function, value)?))
    }

    /// Records `function` = `value` as sent (by the mapping thread, through
    /// the [`SetbfreeSink`]); returns the messages for its GUI, as
    /// [`Setbfree::set`].
    pub(crate) fn record(&mut self, function: &str, value: u8) -> Option<Vec<PortEvent>> {
        let value = value.min(127);
        let mut for_gui = vec![self.message(self.notify_port, function, value)?];
        self.values.insert(function.to_string(), value);
        if function == SPEED_PRESET {
            let select = speed_select(value);
            for_gui.push(self.message(self.notify_port, SPEED_SELECT, select)?);
            self.values.insert(SPEED_SELECT.to_string(), select);
        }
        Some(for_gui)
    }

    /// A control message for `port`.
    fn message(&self, port: u32, function: &str, value: u8) -> Option<PortEvent> {
        control_message(self.urids, port, function, value)
    }

    pub(crate) fn value(&self, function: &str) -> Option<u8> {
        self.values.get(function).copied()
    }

    /// Records what `event` says, if it's a control message (from the plugin,
    /// or from its GUI to the plugin). Returns whether a value changed.
    pub(crate) fn observe(&mut self, event: &PortEvent) -> bool {
        let Some((function, value)) = self.decode(event) else { return false };
        log::trace!("setBfree: {function} = {value}");
        if function == REINIT {
            let had_values = !self.values.is_empty();
            self.values.clear();
            return had_values;
        }
        // Housekeeping (`special.init`, `special.midimap`), not settings.
        if function.starts_with("special.") {
            return false;
        }
        let value = value.clamp(0, 127) as u8;
        self.values.insert(function, value) != Some(value)
    }

    /// `(function, value)` of a control message.
    fn decode(&self, event: &PortEvent) -> Option<(String, i32)> {
        if event.atom_type != self.urids.object {
            return None;
        }
        let body = event.body.get(..event.size as usize)?;
        // LV2_Atom_Object_Body: id, otype; then the properties.
        if read_u32(body, 4)? != self.urids.controlmsg {
            return None;
        }
        let (mut key, mut value) = (None, None);
        let mut offset = 8;
        // LV2_Atom_Property_Body: key, context, value atom (size, type, body),
        // each property padded to 8 bytes.
        while offset + 16 <= body.len() {
            let property = read_u32(body, offset)?;
            let size = read_u32(body, offset + 8)? as usize;
            let atom_type = read_u32(body, offset + 12)?;
            let data = body.get(offset + 16..offset + 16 + size)?;
            if property == self.urids.controlkey && atom_type == self.urids.string {
                let text = data.split(|&b| b == 0).next().unwrap_or_default();
                key = Some(String::from_utf8_lossy(text).into_owned());
            } else if property == self.urids.controlval && atom_type == self.urids.int {
                value = Some(i32::from_ne_bytes(data.get(..4)?.try_into().ok()?));
            }
            offset += 16 + pad8(size);
        }
        Some((key?, value.unwrap_or(0)))
    }
}

/// A control message setting `function` to `value`, for `port`.
fn control_message(urids: Urids, port: u32, function: &str, value: u8) -> Option<PortEvent> {
    let mut writer = ObjectWriter::new(port, urids.object, urids.controlmsg);
    let mut key = function.as_bytes().to_vec();
    key.push(0);
    writer.property(urids.controlkey, urids.string, &key)?;
    writer.property(urids.controlval, urids.int, &i32::from(value).to_ne_bytes())?;
    Some(writer.finish())
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn pad8(size: usize) -> usize {
    size.div_ceil(8) * 8
}

/// Builds an `atom:Object` as a [`PortEvent`] body.
struct ObjectWriter {
    event: PortEvent,
    len: usize,
}

impl ObjectWriter {
    fn new(port: u32, object: u32, otype: u32) -> Self {
        let mut event = PortEvent { port, atom_type: object, size: 0, body: [0; PORT_EVENT_BYTES] };
        // id 0 (a blank object), then the type.
        event.body[4..8].copy_from_slice(&otype.to_ne_bytes());
        Self { event, len: 8 }
    }

    fn property(&mut self, key: u32, atom_type: u32, data: &[u8]) -> Option<()> {
        let end = self.len + 16 + pad8(data.len());
        let body = self.event.body.get_mut(self.len..end)?;
        body[0..4].copy_from_slice(&key.to_ne_bytes());
        body[8..12].copy_from_slice(&(data.len() as u32).to_ne_bytes());
        body[12..16].copy_from_slice(&atom_type.to_ne_bytes());
        body[16..16 + data.len()].copy_from_slice(data);
        self.len = end;
        Some(())
    }

    fn finish(&mut self) -> PortEvent {
        self.event.size = self.len as u32;
        self.event
    }
}

#[cfg(test)]
mod tests {
    use super::speed_select;

    #[test]
    fn speed_preset_to_select() {
        // Slow, stop, fast: horn and drum both slow, both stopped, both fast.
        assert_eq!([0, 64, 127].map(speed_select), [64, 0, 127]);
        // The preset's ranges are v / 43.
        assert_eq!([42, 43, 85, 86].map(speed_select), [64, 0, 0, 127]);
    }
}
