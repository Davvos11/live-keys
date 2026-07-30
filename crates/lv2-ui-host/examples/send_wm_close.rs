//! Throwaway test helper: finds a top-level window by exact title and sends it a
//! WM_DELETE_WINDOW ClientMessage, exactly as a window manager does when the user
//! clicks the window's own close button. Used to reproduce/verify the
//! `ui_closed`-on-user-close path without needing to click through the UI manually.
//! Usage: `cargo run --example send_wm_close -- "Helm"`

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt, EventMask};

fn find_window_by_title(
    conn: &impl Connection,
    root: u32,
    title: &str,
) -> anyhow::Result<Option<u32>> {
    let tree = conn.query_tree(root)?.reply()?;
    for child in tree.children {
        if let Ok(prop) = conn
            .get_property(false, child, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)?
            .reply()
            && !prop.value.is_empty()
        {
            let name = String::from_utf8_lossy(&prop.value);
            if std::env::var("DEBUG_LIST").is_ok() {
                eprintln!("{child:#x}: {name:?}");
            }
            if name == title {
                return Ok(Some(child));
            }
        }
        if let Some(found) = find_window_by_title(conn, child, title)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

fn main() -> anyhow::Result<()> {
    let title = std::env::args().nth(1).expect("usage: send_wm_close <title>");
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;

    let window = find_window_by_title(&conn, root, &title)?
        .ok_or_else(|| anyhow::anyhow!("no window titled '{title}' found"))?;
    println!("found window {window:#x}, sending WM_DELETE_WINDOW");

    let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let wm_delete_window = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;

    let event = ClientMessageEvent::new(
        32,
        window,
        wm_protocols,
        [wm_delete_window, x11rb::CURRENT_TIME, 0, 0, 0],
    );
    conn.send_event(false, window, EventMask::NO_EVENT, event)?;
    conn.flush()?;
    println!("sent");
    Ok(())
}
