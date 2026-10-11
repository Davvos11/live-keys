//! Looking at and resizing plugin windows from outside, as a user would.

use std::collections::HashSet;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, ImageFormat};
use x11rb::rust_connection::RustConnection;

pub struct Display {
    conn: RustConnection,
}

impl Display {
    pub fn connect() -> Result<Self> {
        let (conn, _) = x11rb::connect(None).context("failed to connect to X server")?;
        Ok(Self { conn })
    }

    pub fn size(&self, window: u32) -> Result<(u32, u32)> {
        let geometry = self.conn.get_geometry(window)?.reply()?;
        Ok((u32::from(geometry.width), u32::from(geometry.height)))
    }

    /// The first window embedded in `window` (the plugin's widget).
    pub fn child(&self, window: u32) -> Result<Option<u32>> {
        Ok(self.conn.query_tree(window)?.reply()?.children.first().copied())
    }

    pub fn resize(&self, window: u32, (width, height): (u32, u32)) -> Result<()> {
        self.conn.configure_window(window, &ConfigureWindowAux::new().width(width).height(height))?;
        self.conn.flush()?;
        Ok(())
    }

    /// How many distinct colours a sample of `window`'s pixels has (capped at
    /// 64): 1 or 2 means nothing was drawn (a blank white window).
    pub fn colours(&self, window: u32) -> Result<usize> {
        let (width, height) = self.size(window)?;
        let image = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, window, 0, 0, width as u16, height as u16, !0)?
            .reply()
            .context("couldn't read the window's pixels (is it mapped and on screen?)")?;
        let mut seen = HashSet::new();
        for pixel in image.data.chunks_exact(4).step_by(37) {
            seen.insert([pixel[0], pixel[1], pixel[2]]);
            if seen.len() >= 64 {
                break;
            }
        }
        Ok(seen.len())
    }
}
