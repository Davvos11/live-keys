/// A request to set a plugin parameter to a new value, generated on a control
/// thread (a parameter slider, or a plugin's own GUI) and applied on the audio
/// thread via [`crate::HostedPlugin::set_param`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamChange {
    pub param_id: u32,
    pub value: f32,
}
