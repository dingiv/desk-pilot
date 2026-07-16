//! Renderer abstraction + a zero-dependency CPU baseline.
//!
//! The concrete GPU renderer (wgpu / femtovg / ...) is picked after the survey
//! in docs/rendering.md. Until then this CPU path both proves the pipeline and
//! doubles as the "worst case: minimal own engine" fallback.

use core::{Canvas, Color, PetShape, Scene};

/// Anything that can paint a scene into a canvas.
pub trait Renderer {
    fn render(&self, scene: &Scene, out: &mut Canvas);
}

/// Hand-rolled CPU rasterizer: filled rectangles + midpoint circles.
/// No anti-aliasing, no external crates.
pub struct CpuRenderer;

impl CpuRenderer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CpuRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer for CpuRenderer {
    fn render(&self, scene: &Scene, out: &mut Canvas) {
        out.resize(scene.canvas_size.0, scene.canvas_size.1);
        out.clear(scene.bg);
        match scene.pet.shape {
            PetShape::Rect { w, h } => fill_rect(
                out,
                scene.pet.pos.x - w * scene.pet.scale / 2.0,
                scene.pet.pos.y - h * scene.pet.scale / 2.0,
                w * scene.pet.scale,
                h * scene.pet.scale,
                scene.pet.color,
            ),
            PetShape::Circle { radius } => fill_circle(
                out,
                scene.pet.pos.x,
                scene.pet.pos.y,
                radius * scene.pet.scale,
                scene.pet.color,
            ),
        }
    }
}

fn fill_rect(out: &mut Canvas, x: f32, y: f32, w: f32, h: f32, c: Color) {
    let x0 = x.floor().max(0.0) as i32;
    let y0 = y.floor().max(0.0) as i32;
    let x1 = (x + w).ceil() as i32;
    let y1 = (y + h).ceil() as i32;
    for py in y0..y1 {
        for px in x0..x1 {
            out.put(px, py, c);
        }
    }
}

fn fill_circle(out: &mut Canvas, cx: f32, cy: f32, r: f32, c: Color) {
    let r2 = r * r;
    let x0 = (cx - r).floor() as i32;
    let x1 = (cx + r).ceil() as i32;
    let y0 = (cy - r).floor() as i32;
    let y1 = (cy + r).ceil() as i32;
    for py in y0..y1 {
        for px in x0..x1 {
            let dx = px as f32 - cx;
            let dy = py as f32 - cy;
            if dx * dx + dy * dy <= r2 {
                out.put(px, py, c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{geometry::Vec2, Pet};

    #[test]
    fn renders_centered_circle() {
        let scene = Scene {
            pet: Pet {
                pos: Vec2::new(50.0, 50.0),
                shape: PetShape::Circle { radius: 10.0 },
                color: Color::WHITE,
                scale: 1.0,
            },
            canvas_size: (100, 100),
            bg: Color::BLACK,
        };
        let mut c = Canvas::new(100, 100);
        CpuRenderer::new().render(&scene, &mut c);
        // center pixel should be the circle, not the background
        let i = (50 * 100 + 50) * 4;
        assert_eq!(&c.as_bytes()[i..i + 3], &[0xFF, 0xFF, 0xFF]);
    }
}
