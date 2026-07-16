//! macOS backend — **interface scaffold only**, implementation in M5.
//!
//! Plan: hand-written `NSPanel` subclass; transparency via
//! `self.isOpaque = false` + `backgroundColor = NSColor.clear`; irregular
//! click-through by dynamically toggling `ignoresMouseEvents` (or a tracking
//! rect); topmost via `[self setLevel:NSFloatingWindowLevel]` (and absolute
//! coords via `setFrame`); render via Metal (or ANGLE). See docs/index.md §1.

use crate::window::{InputRegion, KeepAboveMode, KeepAboveResult, PetWindow};
use crate::{App, PlatformBackend};

pub struct MacosBackend;

impl PlatformBackend for MacosBackend {
    fn run(&mut self, _app: Box<dyn App>) -> ! {
        todo!("macOS backend (NSPanel/Metal) — implement in M5")
    }
}

/// NSPanel-backed `PetWindow`. Realized in M5.
pub struct MacosPetWindow;

impl PetWindow for MacosPetWindow {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn set_input_region(&mut self, _region: &InputRegion) {
        // M5: dynamic ignoresMouseEvents toggling.
    }
    fn request_keep_above(&mut self, _mode: KeepAboveMode) -> KeepAboveResult {
        KeepAboveResult::Unsupported // M5: setLevel:NSFloatingWindowLevel
    }
}
