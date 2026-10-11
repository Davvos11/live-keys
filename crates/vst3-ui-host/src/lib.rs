//! Hosts a VST3 plugin's own editor (`IPlugView`) in an X11 window.
//!
//! Unlike LV2 UIs (see `live-keys-lv2-ui`), a VST3 editor can't get a thread of
//! its own: the VST3 threading rules put the editor, like the edit controller it
//! belongs to, on the main thread. So [`Vst3Editor`] does no work by itself; the
//! host calls [`Vst3Editor::idle`] regularly from its main-thread event loop, which
//! pumps the window's X11 events and everything the plugin registered with the
//! host's run loop (`Steinberg::Linux::IRunLoop` — file descriptors to watch and
//! timers, which is how e.g. JUCE plugins drive their own UI on Linux).
//!
//! X11-specific by nature; a Windows/macOS backend would replace this crate.

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_void};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use live_keys_vst3_host::vst3::Steinberg::Linux::{
    IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
    ITimerHandlerTrait,
};
use live_keys_vst3_host::vst3::Steinberg::Vst::IEditControllerTrait;
use live_keys_vst3_host::vst3::Steinberg::{
    FIDString, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, ViewRect,
    kInvalidArgument, kPlatformTypeX11EmbedWindowID, kResultOk, kResultTrue, tresult,
};
use live_keys_vst3_host::vst3::{Class, ComPtr, ComRef, ComWrapper};
use live_keys_vst3_host::{EditCallback, Vst3Controller};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, PropMode,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Used if a plugin reports no usable initial size.
const FALLBACK_SIZE: (u32, u32) = (800, 600);

/// The top-level window an editor is embedded into.
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
        conn.change_property32(
            PropMode::REPLACE,
            id,
            wm_protocols,
            AtomEnum::ATOM,
            &[wm_delete_window],
        )?;
        let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        conn.change_property8(PropMode::REPLACE, id, net_wm_name, utf8_string, title.as_bytes())?;

        let window = Self {
            conn,
            id,
            wm_protocols,
            wm_delete_window,
            size: Cell::new((width, height)),
        };
        if !resizable {
            window.pin_size(width, height)?;
        }
        window.conn.map_window(id)?;
        window.conn.flush()?;
        Ok(window)
    }

    /// Stops the window manager from offering resizes the plugin can't follow.
    fn pin_size(&self, width: u32, height: u32) -> Result<()> {
        let mut hints = WmSizeHints::new();
        hints.min_size = Some((width as i32, height as i32));
        hints.max_size = Some((width as i32, height as i32));
        hints.set_normal_hints(&self.conn, self.id)?;
        Ok(())
    }

    fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.size.set((width, height));
        self.conn.configure_window(
            self.id,
            &ConfigureWindowAux::new().width(width).height(height),
        )?;
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

fn rect_size(rect: &ViewRect) -> Option<(u32, u32)> {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    (width > 0 && height > 0).then_some((width as u32, height as u32))
}

fn size_rect((width, height): (u32, u32)) -> ViewRect {
    ViewRect {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    }
}

struct Timer {
    handler: ComPtr<ITimerHandler>,
    interval: Duration,
    due: Instant,
}

/// The `IPlugFrame` handed to the view, which is also the host's `IRunLoop`:
/// plugins find the run loop by querying their frame for it.
struct PlugFrame {
    window: Rc<HostWindow>,
    event_handlers: RefCell<Vec<(ComPtr<IEventHandler>, c_int)>>,
    timers: RefCell<Vec<Timer>>,
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame, IRunLoop);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        let (Some(view), false) = (unsafe { ComRef::from_raw(view) }, new_size.is_null()) else {
            return kInvalidArgument;
        };
        let Some(size) = rect_size(unsafe { &*new_size }) else {
            return kInvalidArgument;
        };
        if let Err(err) = self.window.resize(size.0, size.1) {
            log::warn!("failed to resize VST3 editor window: {err}");
        }
        unsafe { view.onSize(new_size) };
        kResultOk
    }
}

impl IRunLoopTrait for PlugFrame {
    unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: c_int) -> tresult {
        let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
            return kInvalidArgument;
        };
        self.event_handlers.borrow_mut().push((handler.to_com_ptr(), fd));
        kResultTrue
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        self.event_handlers
            .borrow_mut()
            .retain(|(registered, _)| registered.as_ptr() != handler);
        kResultTrue
    }

    unsafe fn registerTimer(&self, handler: *mut ITimerHandler, milliseconds: u64) -> tresult {
        let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
            return kInvalidArgument;
        };
        let interval = Duration::from_millis(milliseconds.max(1));
        self.timers.borrow_mut().push(Timer {
            handler: handler.to_com_ptr(),
            interval,
            due: Instant::now() + interval,
        });
        kResultTrue
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
        self.timers
            .borrow_mut()
            .retain(|timer| timer.handler.as_ptr() != handler);
        kResultTrue
    }
}

impl PlugFrame {
    /// Runs the handlers of every registered fd that's ready to read, without
    /// blocking.
    fn dispatch_fds(&self) {
        // Snapshot first: a handler may (un)register handlers while running.
        let handlers: Vec<(ComPtr<IEventHandler>, c_int)> = self.event_handlers.borrow().clone();
        if handlers.is_empty() {
            return;
        }
        let mut fds: Vec<libc::pollfd> = handlers
            .iter()
            .map(|&(_, fd)| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) } <= 0 {
            return;
        }
        for ((handler, fd), pollfd) in handlers.iter().zip(&fds) {
            if pollfd.revents != 0 && self.is_registered_handler(handler) {
                unsafe { handler.onFDIsSet(*fd) };
            }
        }
    }

    /// Fires every timer that's due.
    fn dispatch_timers(&self) {
        let now = Instant::now();
        let due: Vec<ComPtr<ITimerHandler>> = self
            .timers
            .borrow_mut()
            .iter_mut()
            .filter(|timer| timer.due <= now)
            .map(|timer| {
                // Skip missed ticks rather than firing a burst to catch up.
                timer.due = now + timer.interval;
                timer.handler.clone()
            })
            .collect();
        for handler in due {
            if self.timers.borrow().iter().any(|timer| timer.handler.as_ptr() == handler.as_ptr()) {
                unsafe { handler.onTimer() };
            }
        }
    }

    fn is_registered_handler(&self, handler: &ComPtr<IEventHandler>) -> bool {
        self.event_handlers
            .borrow()
            .iter()
            .any(|(registered, _)| registered.as_ptr() == handler.as_ptr())
    }
}

/// A VST3 plugin's own editor, open in a window of its own. Main thread only;
/// call [`Vst3Editor::idle`] regularly (every ~16 ms) for it to work at all.
///
/// Must be closed (or dropped) before the plugin it belongs to is unloaded.
pub struct Vst3Editor {
    view: ComPtr<IPlugView>,
    frame: ComWrapper<PlugFrame>,
    window: Rc<HostWindow>,
    closed: bool,
}

impl Vst3Editor {
    /// Opens `controller`'s editor in a new window titled `title`. `on_edit` is
    /// called (on the main thread) whenever the user changes a parameter in it;
    /// it should forward the change to the audio processor. It stays installed
    /// after the editor closes, which is harmless: only an editor makes edits.
    pub fn open(controller: &Vst3Controller, title: &str, on_edit: EditCallback) -> Result<Self> {
        let edit_controller = controller.edit_controller();
        let view = unsafe {
            ComPtr::from_raw(edit_controller.createView(c"editor".as_ptr()))
                .context("this plugin has no editor")?
        };
        let platform: FIDString = kPlatformTypeX11EmbedWindowID;
        anyhow::ensure!(
            unsafe { view.isPlatformTypeSupported(platform) } == kResultTrue,
            "this plugin's editor doesn't support X11 embedding"
        );

        let mut rect: ViewRect = unsafe { std::mem::zeroed() };
        let size = if unsafe { view.getSize(&mut rect) } == kResultOk {
            rect_size(&rect).unwrap_or(FALLBACK_SIZE)
        } else {
            FALLBACK_SIZE
        };
        let resizable = unsafe { view.canResize() } == kResultTrue;
        let window = Rc::new(HostWindow::create(title, size, resizable)?);

        let frame = ComWrapper::new(PlugFrame {
            window: window.clone(),
            event_handlers: RefCell::new(Vec::new()),
            timers: RefCell::new(Vec::new()),
        });
        controller.set_edit_callback(Some(on_edit));
        unsafe {
            view.setFrame(frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr());
            if view.attached(window.id as usize as *mut c_void, platform) != kResultOk {
                view.setFrame(std::ptr::null_mut());
                anyhow::bail!("this plugin's editor failed to attach to its window");
            }
        }

        // Some editors only settle on their size once attached.
        let mut rect: ViewRect = unsafe { std::mem::zeroed() };
        if unsafe { view.getSize(&mut rect) } == kResultOk
            && let Some(actual) = rect_size(&rect)
            && actual != size
        {
            window.resize(actual.0, actual.1)?;
            if !resizable {
                window.pin_size(actual.0, actual.1)?;
            }
        }

        Ok(Self {
            view,
            frame,
            window,
            closed: false,
        })
    }

    /// Pumps the window's events and the plugin's run-loop fds and timers. Closes
    /// the editor if the user closed its window.
    pub fn idle(&mut self) {
        if self.closed {
            return;
        }
        while let Ok(Some(event)) = self.window.conn.poll_for_event() {
            match event {
                Event::ClientMessage(message)
                    if message.type_ == self.window.wm_protocols
                        && message.data.as_data32()[0] == self.window.wm_delete_window =>
                {
                    self.close();
                    return;
                }
                Event::ConfigureNotify(configure) => {
                    let size = (u32::from(configure.width), u32::from(configure.height));
                    if size != self.window.size.get() {
                        self.window.size.set(size);
                        self.follow_window_resize(size);
                    }
                }
                _ => {}
            }
        }
        self.frame.dispatch_fds();
        self.frame.dispatch_timers();
    }

    /// The user resized the window: let the view follow, within its constraints.
    fn follow_window_resize(&self, size: (u32, u32)) {
        if unsafe { self.view.canResize() } != kResultTrue {
            return;
        }
        let mut rect = size_rect(size);
        unsafe {
            self.view.checkSizeConstraint(&mut rect);
            self.view.onSize(&mut rect);
        }
        if let Some(constrained) = rect_size(&rect)
            && constrained != size
        {
            let _ = self.window.resize(constrained.0, constrained.1);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The X11 window the editor is embedded in (for tests resizing it).
    pub fn window(&self) -> u32 {
        self.window.id
    }

    /// Detaches the editor and destroys its window. A no-op if already closed.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
        }
        self.frame.event_handlers.borrow_mut().clear();
        self.frame.timers.borrow_mut().clear();
        let _ = self.window.conn.unmap_window(self.window.id);
        let _ = self.window.conn.flush();
    }
}

impl Drop for Vst3Editor {
    fn drop(&mut self) {
        self.close();
    }
}
