use livi::lilv::ui::UI;

/// Which LV2 UI hosting mechanism a plugin's own GUI needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKind {
    /// The kxstudio "external-ui" extension: the plugin manages its own top-level
    /// window entirely; the host just calls `show`/`hide`/`run`. No window
    /// embedding required.
    ExternalUi,
    /// A plain X11 window the host must create and embed the plugin's widget into.
    X11Ui,
}

const EXTERNAL_UI_CLASS: &str = "http://kxstudio.sf.net/ns/lv2ext/external-ui#Widget";
const X11_UI_CLASS: &str = "http://lv2plug.in/ns/extensions/ui#X11UI";

fn ui_is_a(ui: &UI, class_uri: &str) -> bool {
    ui.classes().iter().any(|c| c.as_uri() == Some(class_uri))
}

/// Picks the best UI this host can show for `plugin`, preferring [`UiKind::ExternalUi`]
/// (no window-embedding required) over [`UiKind::X11Ui`]. Returns `None` if the plugin
/// has no UI this host knows how to drive (e.g. it's headless, or only offers a
/// toolkit-specific UI type like Gtk/Qt that isn't wrapped here).
pub fn best_supported_ui(plugin: &livi::Plugin) -> Option<(UI, UiKind)> {
    let uis = plugin.raw().uis()?;
    let mut x11 = None;
    for ui in uis {
        if ui_is_a(&ui, EXTERNAL_UI_CLASS) {
            return Some((ui, UiKind::ExternalUi));
        }
        if x11.is_none() && ui_is_a(&ui, X11_UI_CLASS) {
            x11 = Some(ui);
        }
    }
    x11.map(|ui| (ui, UiKind::X11Ui))
}
