//! Platform abstraction: owns the window + frame clock, drives the app loop,
//! and is deliberately ignorant of rendering. Each frame it hands the App a
//! `Canvas` to paint (the App uses its own Renderer); the platform just blits
//! `out.as_bytes()` to its window surface.

pub mod event;
pub mod headless;
pub mod window;

#[cfg(feature = "gtk")]
pub mod gtk;
/// Linux/Wayland keep-above strategies (layer-shell vs GNOME extension).
#[cfg(feature = "gtk")]
pub mod keep_above;
/// Headless egui renderer (offscreen wgpu → RGBA8) for the GTK backend.
#[cfg(all(feature = "gtk", feature = "egui"))]
pub(crate) mod gtk_egui;
/// egui binder for the declarative `ui::View` tree (the render layer).
#[cfg(all(feature = "gtk", feature = "egui"))]
pub(crate) mod egui_view;
#[cfg(feature = "windows")]
pub mod windows;
#[cfg(feature = "macos")]
pub mod macos;

pub use event::{MouseButton, PlatformEvent};
pub use headless::HeadlessBackend;
pub use window::{InputRegion, KeepAboveMode, KeepAboveResult, PetWindow};

use core::Canvas;

/// The application: consumes input events, advances logic, paints a frame.
pub trait App {
    fn canvas_size(&self) -> (u32, u32);
    fn handle_event(&mut self, ev: &PlatformEvent);
    fn tick(&mut self, dt: std::time::Duration);
    /// Paint into `out`, which is sized to `canvas_size()`.
    fn render(&self, out: &mut Canvas);
    /// The canvas-pixel region that should *capture* pointer input (the pet's
    /// silhouette); everything else is click-through to windows below. Default is
    /// empty = full pass-through. Backends re-apply this each frame as the pet
    /// moves. Coordinates are canvas px (window-local at scale 1).
    fn input_region(&self) -> InputRegion {
        InputRegion::default()
    }

    /// Declare the UI as a pure [`ui::View`] tree (the egui/declarative path).
    /// Default: an empty UI. Built fresh each frame from state; the platform
    /// renders it and routes interactions back via [`App::update`].
    fn view(&self) -> ui::View {
        ui::View::default()
    }

    /// React to a UI interaction ([`ui::Msg`]). Default: ignore.
    fn update(&mut self, _msg: ui::Msg) {}
}

/// Owns the window + frame clock and runs until the user closes it.
pub trait PlatformBackend {
    /// Blocks; never returns normally (exits the process or loops forever).
    /// Takes ownership so backends (e.g. GTK's closure-driven loop) can share
    /// the app across callbacks via `Rc<RefCell<…>>`.
    fn run(&mut self, app: Box<dyn App>) -> !;
}
