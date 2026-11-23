//! Effects' inline displays (Ardour's LV2 extension, see `live-keys-lv2-host`) in
//! their FX squares: rendered on the poll timer for plugins that asked to be
//! redrawn, throttled, and only while their square's tab shows.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use live_keys_engine::{Engine, InlinePicture, PluginKey, SLOT_COUNT};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// The shortest time between two renders of one plugin's display (~30 fps).
const MIN_INTERVAL: Duration = Duration::from_millis(33);
/// The area a display may fill: an FX square (80 px) inside its 1 px border.
const DISPLAY_SIZE: f32 = 78.0;

/// A slot's tabs, as `SlotPanel`'s `tab`: which of its effects show.
const TAB_PANEL: i32 = 0;
const TAB_MIDI: i32 = 1;

struct Rendered {
    /// `None` when the plugin had nothing to show.
    image: Option<Image>,
    at: Instant,
    /// The window's scale factor it was rendered for.
    scale: f32,
}

/// The last picture per plugin and which tab each slot shows.
pub struct InlineDisplays {
    rendered: HashMap<PluginKey, Rendered>,
    tabs: [i32; SLOT_COUNT],
}

impl InlineDisplays {
    pub fn new() -> Self {
        Self {
            rendered: HashMap::new(),
            tabs: [TAB_PANEL; SLOT_COUNT],
        }
    }

    pub fn set_tab(&mut self, slot: usize, tab: i32) {
        if let Some(current) = self.tabs.get_mut(slot) {
            *current = tab;
        }
    }

    /// Whether a slot's audio effects (`midi: false`) or MIDI effects show; the
    /// master chain's always do.
    pub fn visible(&self, slot: usize, midi: bool) -> bool {
        self.tabs.get(slot).is_none_or(|&tab| tab == if midi { TAB_MIDI } else { TAB_PANEL })
    }

    /// `key`'s last picture, for its chain row.
    pub fn image(&self, key: PluginKey) -> Option<Image> {
        self.rendered.get(&key).and_then(|rendered| rendered.image.clone())
    }

    pub fn forget(&mut self, key: PluginKey) {
        self.rendered.remove(&key);
    }

    pub fn clear(&mut self) {
        self.rendered.clear();
    }

    /// Renders `key`'s display if it asked to be redrawn (or the scale factor
    /// changed since) and the last render is long enough ago. Returns whether
    /// its picture changed.
    pub fn update(&mut self, engine: &Engine, key: PluginKey, scale: f32, now: Instant) -> bool {
        let previous = self.rendered.get(&key);
        let rescaled = previous.is_some_and(|rendered| rendered.scale != scale);
        if !(engine.inline_display_dirty(key) || rescaled)
            || previous.is_some_and(|rendered| now.duration_since(rendered.at) < MIN_INTERVAL)
        {
            return false;
        }
        let size = (DISPLAY_SIZE * scale).round() as u32;
        let image = engine.render_inline_display(key, size, size).map(to_image);
        let changed = image.is_some() || previous.is_some_and(|rendered| rendered.image.is_some());
        self.rendered.insert(key, Rendered { image, at: now, scale });
        changed
    }
}

fn to_image(picture: InlinePicture) -> Image {
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&picture.rgba, picture.width, picture.height);
    Image::from_rgba8_premultiplied(buffer)
}
