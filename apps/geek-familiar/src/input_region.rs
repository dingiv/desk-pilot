//! Alpha-derived input region (click-through passthrough) for a Wayland surface.
//!
//! Scan the alpha channel of a rendered frame → opaque rects → set them as the
//! wl_surface input region, so clicks on transparent areas fall through to windows
//! below. Framework-agnostic: [`apply`] takes raw `wl_surface` / `wl_display`
//! pointers (extracted from the UI framework's raw window handle by the caller).

use std::ffi::c_void;

#[cfg(target_os = "linux")]
use wayland_client::{
    backend::Backend,
    protocol::{wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry::WlRegistry, wl_surface::WlSurface},
    Connection, Dispatch, Proxy, QueueHandle,
};
#[cfg(target_os = "linux")]
use wayland_client::globals::GlobalListContents;

/// A rectangle in surface (physical pixel) coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Scan the alpha channel of an RGBA frame and return rects covering opaque areas
/// (alpha >= `threshold`). One rect per maximal horizontal run per row.
pub fn alpha_rects(rgba: &[u8], width: u32, height: u32, threshold: u8) -> Vec<Rect> {
    if width == 0 || height == 0 || rgba.is_empty() {
        return Vec::new();
    }

    let mut rects = Vec::new();
    let stride = width * 4;

    for y in 0..height {
        let row_start = (y * stride) as usize;
        let mut x_start: Option<u32> = None;

        for x in 0..width {
            let pixel_offset = row_start + (x * 4) as usize;
            let alpha = rgba[pixel_offset + 3];

            if alpha >= threshold {
                if x_start.is_none() {
                    x_start = Some(x);
                }
            } else if let Some(start) = x_start {
                rects.push(Rect { x: start as i32, y: y as i32, w: (x - start) as i32, h: 1 });
                x_start = None;
            }
        }

        if let Some(start) = x_start {
            rects.push(Rect { x: start as i32, y: y as i32, w: (width - start) as i32, h: 1 });
        }
    }

    const MAX_RECTS: usize = 4000;
    if rects.len() > MAX_RECTS {
        eprintln!(
            "[passthrough] WARNING: {count} rects exceeded cap {MAX_RECTS}, truncating",
            count = rects.len()
        );
        rects.truncate(MAX_RECTS);
    }

    rects
}

/// Set `rects` as the input region of `surface` so clicks outside them pass through.
///
/// # Safety
/// `surface` and `display` must be valid `wl_surface` / `wl_display` pointers owned
/// by another (live) connection. The surface is FOREIGN — this must NOT destroy it.
#[cfg(target_os = "linux")]
pub unsafe fn apply(surface: *mut c_void, display: *mut c_void, rects: &[Rect]) {
    use wayland_client::backend;

    // Safety: The caller guarantees these are valid pointers from a live Wayland connection
    let backend = unsafe { Backend::from_foreign_display(display as *mut _) };

    let conn = Connection::from_backend(backend);

    // State type for event queue - no-op implementations are sufficient
    struct ShellState;

    impl Dispatch<WlCompositor, ()> for ShellState {
        fn event(
            _state: &mut Self,
            _proxy: &WlCompositor,
            _event: <WlCompositor as wayland_client::Proxy>::Event,
            _data: &(),
            _connhandle: &wayland_client::Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<WlRegion, ()> for ShellState {
        fn event(
            _state: &mut Self,
            _proxy: &WlRegion,
            _event: <WlRegion as wayland_client::Proxy>::Event,
            _data: &(),
            _connhandle: &wayland_client::Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<WlRegistry, GlobalListContents> for ShellState {
        fn event(
            _state: &mut Self,
            _proxy: &WlRegistry,
            _event: <WlRegistry as wayland_client::Proxy>::Event,
            _data: &GlobalListContents,
            _connhandle: &wayland_client::Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    // Create event queue and initialize registry
    let (globals, mut queue) = match wayland_client::globals::registry_queue_init(&conn) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[passthrough] Failed to initialize registry queue: {:?}", e);
            return;
        }
    };

    let qh = queue.handle();

    // Bind wl_compositor
    let compositor: WlCompositor = match globals.bind(&qh, 1..=4, ()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[passthrough] Failed to bind wl_compositor: {:?}", e);
            return;
        }
    };

    // Create region and add rectangles
    let region = compositor.create_region(&qh, ());

    for r in rects {
        region.add(r.x, r.y, r.w, r.h);
    }

    // Wrap the foreign surface and set input region
    let iface = <WlSurface as Proxy>::interface();
    let surface_id = match backend::ObjectId::from_ptr(iface, surface as *mut _) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[passthrough] Failed to create surface ObjectId: {:?}", e);
            region.destroy();
            return;
        }
    };

    let foreign_surface = match WlSurface::from_id(&conn, surface_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[passthrough] Failed to create surface proxy: {:?}", e);
            region.destroy();
            return;
        }
    };

    foreign_surface.set_input_region(Some(&region));

    // CRITICAL SAFETY: Do NOT destroy the foreign surface - iced owns it
    std::mem::forget(foreign_surface);

    // The region is ours to destroy
    region.destroy();

    // Flush the request
    let _ = queue.roundtrip(&mut ShellState);
}

/// No-op stub for non-Linux platforms
#[cfg(not(target_os = "linux"))]
#[allow(unused_variables)]
pub unsafe fn apply(surface: *mut c_void, display: *mut c_void, rects: &[Rect]) {
    // No-op on non-Wayland platforms
}
