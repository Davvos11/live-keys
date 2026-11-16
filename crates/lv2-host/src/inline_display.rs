//! Ardour's LV2 inline-display extension (`http://harrisonconsoles.com/lv2/inlinedisplay`):
//! a small live picture of the plugin (a compressor's curve, an EQ's response)
//! that the host shows in its mixer strip — for us, an FX square.
//!
//! The host offers the `queue_draw` feature, which the plugin calls from `run()`
//! when its picture changed; the host then calls the plugin's `render` (extension
//! data) on a non-real-time thread, while `run()` may be processing — that's how
//! Ardour does it, and the plugins (x42, LSP) synchronise their own state for it.
//! The picture is a premultiplied, native-endian Cairo ARGB32 surface owned by the
//! plugin.

use std::ffi::{CStr, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lv2_raw::core::{LV2Feature, LV2Handle};

use crate::InstanceRef;

const INTERFACE: &CStr = c"http://harrisonconsoles.com/lv2/inlinedisplay#interface";
const QUEUE_DRAW: &CStr = c"http://harrisonconsoles.com/lv2/inlinedisplay#queue_draw";

/// `LV2_Inline_Display`: the `queue_draw` feature's data.
#[repr(C)]
struct QueueDrawData {
    handle: *mut c_void,
    queue_draw: unsafe extern "C" fn(handle: *mut c_void),
}

/// `LV2_Inline_Display_Image_Surface`.
#[repr(C)]
struct Surface {
    data: *mut u8,
    width: c_int,
    height: c_int,
    stride: c_int,
}

type RenderFn = unsafe extern "C" fn(instance: LV2Handle, max_width: u32, max_height: u32) -> *mut Surface;

/// `LV2_Inline_Display_Interface`.
#[repr(C)]
struct Interface {
    render: Option<RenderFn>,
}

/// Called by the plugin from `run()`, on the audio thread: only sets the flag.
unsafe extern "C" fn queue_draw(handle: *mut c_void) {
    // SAFETY: `handle` is the `AtomicBool` of the `QueueDraw` this feature was
    // made from, which lives as long as the instance (see `Lv2Plugin`'s `Drop`).
    unsafe { (*handle.cast::<AtomicBool>()).store(true, Ordering::Relaxed) };
}

/// The host's side of `queue_draw` for one plugin instance; the feature's data
/// points into it, so it must outlive the instance.
pub(crate) struct QueueDraw {
    dirty: Arc<AtomicBool>,
    data: Box<QueueDrawData>,
}

// SAFETY: the data's handle points at the `AtomicBool` in `dirty`; nothing in
// it is tied to a thread.
unsafe impl Send for QueueDraw {}

impl QueueDraw {
    pub fn new() -> Self {
        // Set, so the picture is rendered once at load even if the plugin only
        // asks after a change.
        let dirty = Arc::new(AtomicBool::new(true));
        let data = Box::new(QueueDrawData {
            handle: Arc::as_ptr(&dirty).cast_mut().cast(),
            queue_draw,
        });
        Self { dirty, data }
    }

    /// The feature to offer at instantiation; valid while `self` lives.
    pub fn feature(&self) -> LV2Feature {
        LV2Feature {
            uri: QUEUE_DRAW.as_ptr(),
            data: (&*self.data as *const QueueDrawData).cast_mut().cast(),
        }
    }

    /// The plugin's inline display, if it has one.
    pub fn display(&self, instance: InstanceRef) -> Option<InlineDisplay> {
        // SAFETY: `extension_data` may be called at any time, from any thread.
        let render = unsafe {
            let descriptor = instance.descriptor.as_ref()?;
            let interface = descriptor.extension_data?(INTERFACE.as_ptr()).cast::<Interface>();
            interface.as_ref()?.render?
        };
        Some(InlineDisplay {
            render,
            handle: instance.handle,
            dirty: self.dirty.clone(),
        })
    }
}

/// A plugin's inline display: whether it asked to be redrawn, and rendering it.
/// Main thread only, in practice (see [`InlineDisplay::render`]).
#[derive(Clone)]
pub struct InlineDisplay {
    render: RenderFn,
    handle: LV2Handle,
    dirty: Arc<AtomicBool>,
}

// SAFETY: only a function pointer and the instance handle, which `render`'s
// caller guarantees is still alive; the plugin expects `render` from a thread
// other than `run()`'s.
unsafe impl Send for InlineDisplay {}

impl InlineDisplay {
    /// Whether the plugin asked to be redrawn since the last [`render`](Self::render).
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Renders the picture, at most `max_width` × `max_height` pixels (the plugin
    /// picks its own size within that, usually the full width), as premultiplied
    /// RGBA. `None` if the plugin has nothing to show.
    ///
    /// # Safety
    /// The plugin instance must still exist, and `render` must not be called
    /// from two threads at once (the plugin keeps one surface).
    pub unsafe fn render(&self, max_width: u32, max_height: u32) -> Option<InlinePicture> {
        // Cleared first, so a request made while rendering isn't lost.
        self.dirty.store(false, Ordering::Relaxed);
        // SAFETY: per this function's contract; the surface and its pixels are
        // the plugin's, valid until the next `render` (copied out at once).
        unsafe {
            let surface = (self.render)(self.handle, max_width, max_height).as_ref()?;
            let (width, height, stride) = (surface.width, surface.height, surface.stride);
            if surface.data.is_null() || width <= 0 || height <= 0 || stride < width * 4 {
                return None;
            }
            let (width, height, stride) = (width as usize, height as usize, stride as usize);
            let len = stride * (height - 1) + width * 4;
            let data = std::slice::from_raw_parts(surface.data, len);
            Some(InlinePicture {
                width: width as u32,
                height: height as u32,
                rgba: argb32_to_rgba(data, width, height, stride),
            })
        }
    }
}

/// One rendered inline display: `width` × `height` premultiplied RGBA pixels.
pub struct InlinePicture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Cairo's ARGB32 (each pixel a native-endian `u32`, alpha in the top byte,
/// premultiplied; rows `stride` bytes apart) as tightly packed RGBA bytes, still
/// premultiplied.
pub fn argb32_to_rgba(data: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let start = row * stride;
        for pixel in data[start..start + width * 4].chunks_exact(4) {
            let argb = u32::from_ne_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
            rgba.extend_from_slice(&[(argb >> 16) as u8, (argb >> 8) as u8, argb as u8, (argb >> 24) as u8]);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb32_reorders_channels_and_skips_row_padding() {
        // 2×2 pixels, rows padded to 12 bytes.
        let pixels: [u32; 4] = [0xff10_2030, 0x8040_0000, 0x0000_0000, 0xff00_00ff];
        let mut data = Vec::new();
        for row in pixels.chunks(2) {
            for pixel in row {
                data.extend_from_slice(&pixel.to_ne_bytes());
            }
            data.extend_from_slice(&[0xee; 4]);
        }
        assert_eq!(
            argb32_to_rgba(&data, 2, 2, 12),
            [0x10, 0x20, 0x30, 0xff, 0x40, 0x00, 0x00, 0x80, 0, 0, 0, 0, 0x00, 0x00, 0xff, 0xff]
        );
    }
}
