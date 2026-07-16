//! Platform events, normalized across backends.

use core::geometry::Vec2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug)]
pub enum PlatformEvent {
    PointerMove { pos: Vec2 },
    PointerDown { button: MouseButton, pos: Vec2 },
    PointerUp { button: MouseButton, pos: Vec2 },
    /// Surface resized, in pixels.
    Resize { width: u32, height: u32 },
    /// Close requested.
    Close,
}

impl MouseButton {
    pub fn is_left(self) -> bool {
        matches!(self, MouseButton::Left)
    }
}
