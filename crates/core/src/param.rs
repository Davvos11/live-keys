/// A request to set an LV2 control-port input to a new value, generated on a control
/// thread (typically from a plugin's own GUI) and applied on the audio thread via
/// [`crate::HostedPlugin::set_control`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamChange {
    pub port_index: u32,
    pub value: f32,
}
