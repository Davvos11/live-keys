//! Hosts a CLAP plugin's own GUI (`clap.gui`, X11 window API): embedded in an
//! X11 window of ours, or — for a plugin that only offers that — in a floating
//! window of its own.
//!
//! Like a VST3 editor, a CLAP GUI lives on the main thread: [`ClapEditor`] does
//! no work by itself, and the host calls [`ClapEditor::idle`] regularly for the
//! window's events. The plugin's timers and fds, which its GUI runs on, are
//! pumped by `ClapController::idle` (the host calls that for every plugin, GUI
//! open or not).
//!
//! X11-specific by nature; a Windows/macOS backend would replace this crate.

use std::cell::Cell;

use anyhow::{Context, Result};
use live_keys_clap_host::{ClapController, ClapGui};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Used if a plugin reports no usable initial size.
const FALLBACK_SIZE: (u32, u32) = (800, 600);

/// The top-level window an embedded GUI goes into.
struct HostWindow {
    conn: RustConnection,
    id: u32,
    wm_protocols: u32,
    wm_delete_window: u32,
    /// Last size we set or were told about, to tell real resizes apart from moves
    /// and from the echo of our own resizes.
    size: Cell<(u32, u32)>,
}

impl HostWindow {
    fn create(title: &str, (width, height): (u32, u32), resizable: bool) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("failed to connect to X server")?;
        let screen = &conn.setup().roots[screen_num];
        let id = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            screen.root,
            0,
            0,
            width as u16,
            height as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(screen.black_pixel)
                .event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
        let wm_delete_window = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
        conn.change_property32(PropMode::REPLACE, id, wm_protocols, AtomEnum::ATOM, &[wm_delete_window])?;
        let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        conn.change_property8(PropMode::REPLACE, id, net_wm_name, utf8_string, title.as_bytes())?;
        let window = Self { conn, id, wm_protocols, wm_delete_window, size: Cell::new((width, height)) };
        if !resizable {
            window.pin_size(width, height)?;
        }
        Ok(window)
    }

    fn map(&self) -> Result<()> {
        self.conn.map_window(self.id)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Stops the window manager from offering resizes the GUI can't follow.
    fn pin_size(&self, width: u32, height: u32) -> Result<()> {
        let mut hints = WmSizeHints::new();
        hints.min_size = Some((width as i32, height as i32));
        hints.max_size = Some((width as i32, height as i32));
        hints.set_normal_hints(&self.conn, self.id)?;
        Ok(())
    }

    fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.size.set((width, height));
        self.conn.configure_window(self.id, &ConfigureWindowAux::new().width(width).height(height))?;
        self.conn.flush()?;
        Ok(())
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.id);
        let _ = self.conn.flush();
    }
}

/// A CLAP plugin's own GUI, open. Main thread only; call [`ClapEditor::idle`]
/// regularly (every ~16 ms).
pub struct ClapEditor {
    gui: ClapGui,
    /// `None` for a floating GUI, which has a window of its own. Boxed: an X11
    /// connection is large.
    window: Option<Box<HostWindow>>,
    resizable: bool,
    closed: bool,
}

impl ClapEditor {
    /// Opens `controller`'s GUI in a new window titled `title`. Edits made in it
    /// reach the host through `ClapController::idle`.
    pub fn open(controller: &ClapController, title: &str) -> Result<Self> {
        let gui = controller.gui().context("this plugin has no GUI")?;
        if gui.is_api_supported(false) {
            Self::open_embedded(gui, title)
        } else if gui.is_api_supported(true) {
            anyhow::ensure!(gui.create(true), "the plugin failed to create its GUI");
            gui.suggest_title(title);
            if !gui.show() {
                gui.destroy();
                anyhow::bail!("the plugin failed to show its GUI");
            }
            Ok(Self { gui, window: None, resizable: false, closed: false })
        } else {
            anyhow::bail!("this plugin's GUI doesn't support X11")
        }
    }

    fn open_embedded(gui: ClapGui, title: &str) -> Result<Self> {
        anyhow::ensure!(gui.create(false), "the plugin failed to create its GUI");
        let size = gui.size().unwrap_or(FALLBACK_SIZE);
        let resizable = gui.can_resize();
        log::debug!("CLAP GUI '{title}': {}×{}, resizable: {resizable}", size.0, size.1);
        let window = match HostWindow::create(title, size, resizable) {
            Ok(window) => window,
            Err(err) => {
                gui.destroy();
                return Err(err);
            }
        };
        if !gui.set_parent(window.id) {
            gui.destroy();
            anyhow::bail!("the plugin's GUI failed to embed into its window");
        }
        window.map()?;
        // Some GUIs only settle on their size once embedded.
        if let Some(actual) = gui.size()
            && actual != size
        {
            window.resize(actual.0, actual.1)?;
            if !resizable {
                window.pin_size(actual.0, actual.1)?;
            }
        }
        if !gui.show() {
            log::warn!("CLAP GUI '{title}' didn't confirm it's shown");
        }
        Ok(Self { gui, window: Some(Box::new(window)), resizable, closed: false })
    }

    /// Pumps the window's events and acts on the GUI's requests (a new size,
    /// its floating window closed). Closes the editor if the user closed its
    /// window.
    pub fn idle(&mut self) {
        if self.closed {
            return;
        }
        if self.gui.take_closed() {
            self.close();
            return;
        }
        let Some(window) = &self.window else { return };
        if let Some((width, height)) = self.gui.take_resize_request()
            && (width, height) != window.size.get()
        {
            if !self.resizable {
                let _ = window.pin_size(width, height);
            }
            if let Err(err) = window.resize(width, height) {
                log::warn!("failed to resize CLAP GUI window: {err}");
            }
        }
        while let Ok(Some(event)) = window.conn.poll_for_event() {
            match event {
                Event::ClientMessage(message)
                    if message.type_ == window.wm_protocols
                        && message.data.as_data32()[0] == window.wm_delete_window =>
                {
                    self.close();
                    return;
                }
                Event::ConfigureNotify(configure) => {
                    let size = (u32::from(configure.width), u32::from(configure.height));
                    if size != window.size.get() {
                        window.size.set(size);
                        self.follow_window_resize(window, size);
                    }
                }
                _ => {}
            }
        }
    }

    /// The user resized the window: let the GUI follow, within its constraints.
    fn follow_window_resize(&self, window: &HostWindow, size: (u32, u32)) {
        if !self.resizable {
            return;
        }
        let adjusted = self.gui.adjust_size(size);
        self.gui.set_size(adjusted);
        if adjusted != size {
            let _ = window.resize(adjusted.0, adjusted.1);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The X11 window the GUI is embedded in (for tests resizing it); `None` for
    /// a floating GUI.
    pub fn window(&self) -> Option<u32> {
        self.window.as_ref().map(|window| window.id)
    }

    /// Hides and destroys the GUI, then its window. A no-op if already closed.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.gui.hide();
        self.gui.destroy();
        self.window = None;
    }
}

impl Drop for ClapEditor {
    fn drop(&mut self) {
        self.close();
    }
}
