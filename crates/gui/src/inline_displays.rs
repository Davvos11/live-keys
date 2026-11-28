//! Effects' inline displays (Ardour's LV2 extension, see `live-keys-lv2-host`) in
//! their FX squares: rendered on the poll timer for plugins that asked to be
//! redrawn, throttled, and only while their square's tab shows.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use live_keys_engine::{Engine, InlinePicture, PluginKey, SLOT_COUNT};
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

/// The shortest time between two renders of one plugin's display (~30 fps).
const MIN_INTERVAL: Duration = Duration::from_millis(33);
/// The area a display may fill: an FX square (80 px) inside its 1 px border.
const DISPLAY_SIZE: f32 = 78.0;

/// A slot's tabs, as `SlotPanel`'s `tab`: which of its effects show.
const TAB_PANEL: i32 = 0;
const TAB_MIDI: i32 = 1;

struct Rendered {
    /// Its row in [`InlineDisplays::pictures`]; `None` when the plugin had
    /// nothing to show.
    row: Option<usize>,
    at: Instant,
    /// The window's scale factor it was rendered for.
    scale: f32,
}

/// The last picture per plugin and which tab each slot shows. The pictures
/// are a model of their own (`AppState.inline-displays`), which a chain row
/// points into (`EffectRow.display`): redrawing one changes only its picture,
/// not the row, so the square's open context menu isn't rebuilt (which empties
/// its submenu).
pub struct InlineDisplays {
    rendered: HashMap<PluginKey, Rendered>,
    tabs: [i32; SLOT_COUNT],
    pictures: Rc<VecModel<Image>>,
    /// Rows of `pictures` no plugin uses.
    free: Vec<usize>,
}

impl InlineDisplays {
    pub fn new() -> Self {
        Self {
            rendered: HashMap::new(),
            tabs: [TAB_PANEL; SLOT_COUNT],
            pictures: Rc::new(VecModel::default()),
            free: Vec::new(),
        }
    }

    /// The pictures, for `AppState.inline-displays`.
    pub fn model(&self) -> ModelRc<Image> {
        ModelRc::from(self.pictures.clone())
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

    /// `key`'s picture for its chain row (`EffectRow.display`): 1 + its row in
    /// the model, 0 for none.
    pub fn display(&self, key: PluginKey) -> i32 {
        self.rendered.get(&key).and_then(|rendered| rendered.row).map_or(0, |row| row as i32 + 1)
    }

    pub fn forget(&mut self, key: PluginKey) {
        if let Some(Rendered { row: Some(row), .. }) = self.rendered.remove(&key) {
            self.release(row);
        }
    }

    pub fn clear(&mut self) {
        let rows: Vec<usize> = self.rendered.drain().filter_map(|(_, rendered)| rendered.row).collect();
        for row in rows {
            self.release(row);
        }
    }

    fn release(&mut self, row: usize) {
        self.pictures.set_row_data(row, Image::default());
        self.free.push(row);
    }

    /// Renders `key`'s display if it asked to be redrawn (or the scale factor
    /// changed since) and the last render is long enough ago, into its
    /// picture. Returns whether its chain row's `display` changed (it got one,
    /// or has none any more), so the row needs updating.
    pub fn update(&mut self, engine: &Engine, key: PluginKey, scale: f32, now: Instant) -> bool {
        let previous = self.rendered.get(&key);
        let rescaled = previous.is_some_and(|rendered| rendered.scale != scale);
        if !(engine.inline_display_dirty(key) || rescaled)
            || previous.is_some_and(|rendered| now.duration_since(rendered.at) < MIN_INTERVAL)
        {
            return false;
        }
        let old_row = previous.and_then(|rendered| rendered.row);
        let size = (DISPLAY_SIZE * scale).round() as u32;
        let row = match (engine.render_inline_display(key, size, size).map(to_image), old_row) {
            (Some(image), Some(row)) => {
                self.pictures.set_row_data(row, image);
                Some(row)
            }
            (Some(image), None) => Some(match self.free.pop() {
                Some(row) => {
                    self.pictures.set_row_data(row, image);
                    row
                }
                None => {
                    self.pictures.push(image);
                    self.pictures.row_count() - 1
                }
            }),
            (None, Some(row)) => {
                self.release(row);
                None
            }
            (None, None) => None,
        };
        self.rendered.insert(key, Rendered { row, at: now, scale });
        row != old_row
    }
}

fn to_image(picture: InlinePicture) -> Image {
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&picture.rgba, picture.width, picture.height);
    Image::from_rgba8_premultiplied(buffer)
}
