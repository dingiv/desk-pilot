//! Small 2D geometry + color primitives.

/// 2D vector / point in canvas (pixel) space. Origin is top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub const fn splat(v: f32) -> Self {
        Self { x: v, y: v }
    }

    pub fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }

    pub fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
    }

    pub fn scale(self, s: f32) -> Self {
        Self::new(self.x * s, self.y * s)
    }
}

/// Axis-aligned rectangle in canvas space (top-left origin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Smallest integer-pixel rect covering this rect (for input regions).
    pub fn to_pixels(&self) -> (i32, i32, i32, i32) {
        let x = self.x.floor() as i32;
        let y = self.y.floor() as i32;
        let x2 = (self.x + self.w).ceil() as i32;
        let y2 = (self.y + self.h).ceil() as i32;
        (x, y, x2 - x, y2 - y)
    }
}

/// 8-bit RGBA color, straight (non-premultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    pub const WHITE: Self = Self::rgba(255, 255, 255, 255);
    pub const BLACK: Self = Self::rgba(0, 0, 0, 255);
    pub const CORAL: Self = Self::rgba(0xFF, 0x6F, 0x61, 0xFF);
    pub const SKY: Self = Self::rgba(0x4F, 0x8E, 0xF1, 0xFF);
}
