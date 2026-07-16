//! Cross-platform interface for the pet's *window*: a transparent, borderless
//! surface whose pointer input can be masked to an irregular region (only the
//! body is clickable; the rest is click-through), with a best-effort
//! "always on top" request.
//!
//! One impl per platform — GTK4/Wayland (Linux), GLFW/WGL (Windows),
//! NSPanel (macOS). The loop/driver is [`PlatformBackend`][crate::PlatformBackend];
//! this trait is the uniform handle for window *properties*.

use core::Rect;

/// Where the window receives pointer events, as window-local pixel rects.
/// Everything outside is click-through to windows below. Empty = full pass-through.
#[derive(Clone, Debug, Default)]
pub struct InputRegion {
    pub rects: Vec<Rect>,
}

impl InputRegion {
    pub fn from_rects(rects: impl IntoIterator<Item = Rect>) -> Self {
        Self {
            rects: rects.into_iter().collect(),
        }
    }
    pub fn single(r: Rect) -> Self {
        Self { rects: vec![r] }
    }
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }
}

/// How strongly we ask to stay above other normal windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepAboveMode {
    /// Don't keep above.
    Off,
    /// Native compositor/OS topmost layer: layer-shell overlay (wlroots/KDE),
    /// `HWND_TOPMOST` (Windows), `NSFloatingWindowLevel` (macOS).
    NativeLayer,
    /// GNOME-only: a companion Shell extension calls `Meta.Window.make_above()`.
    GnomeExtension,
}

/// Whether a keep-above request actually took effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepAboveResult {
    /// Honored.
    Applied,
    /// The platform/compositor can't, or the strategy isn't wired yet.
    Unsupported,
}

/// Uniform window-property interface for the pet, across platforms.
pub trait PetWindow {
    /// Current surface size in pixels.
    fn size(&self) -> (u32, u32);
    /// Mask pointer input to `region` (window-local px); outside = click-through.
    fn set_input_region(&mut self, region: &InputRegion);
    /// Best-effort always-on-top. Returns what the platform could actually do.
    fn request_keep_above(&mut self, mode: KeepAboveMode) -> KeepAboveResult;
}
