//! Windows backend — **interface scaffold only**, implementation in M5.
//!
//! Plan: GLFW/SDL2 borderless window; transparency via `WS_EX_LAYERED` +
//! per-pixel alpha (`UpdateLayeredWindow`); irregular click-through by toggling
//! `WS_EX_TRANSPARENT` to the body region; topmost via `HWND_TOPMOST` +
//! `SetWindowPos` (absolute coords, so native window-move is also available);
//! render via WGL (`wglSwapIntervalEXT(1)` vsync). See docs/index.md §1.

use crate::window::{InputRegion, KeepAboveMode, KeepAboveResult, PetWindow};
use crate::{App, PlatformBackend};

pub struct WindowsBackend;

impl PlatformBackend for WindowsBackend {
    fn run(&mut self, _app: Box<dyn App>) -> ! {
        todo!("Windows backend (GLFW/WGL) — implement in M5")
    }
}

/// HWND-backed `PetWindow`. Realized in M5.
pub struct WindowsPetWindow;

impl PetWindow for WindowsPetWindow {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn set_input_region(&mut self, _region: &InputRegion) {
        // M5: toggle WS_EX_TRANSPARENT for the body region.
    }
    fn request_keep_above(&mut self, _mode: KeepAboveMode) -> KeepAboveResult {
        KeepAboveResult::Unsupported // M5: HWND_TOPMOST via SetWindowPos
    }
}
