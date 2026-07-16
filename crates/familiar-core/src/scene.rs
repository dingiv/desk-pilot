//! Scene model: what the pet is and where it sits, in canvas space.

use crate::geometry::{Color, Rect, Vec2};

/// A programmatic primitive the pet renders as this round (no art assets yet).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PetShape {
    Circle { radius: f32 },
    Rect { w: f32, h: f32 },
}

impl PetShape {
    /// Axis-aligned bounding size, centered at origin.
    pub fn bounds(&self) -> (f32, f32) {
        match self {
            PetShape::Circle { radius } => (radius * 2.0, radius * 2.0),
            PetShape::Rect { w, h } => (*w, *h),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pet {
    /// Center position in canvas pixel space.
    pub pos: Vec2,
    pub shape: PetShape,
    pub color: Color,
    pub scale: f32,
}

impl Pet {
    /// Bounding box in canvas space, accounting for scale.
    pub fn bounding_rect(&self) -> Rect {
        let (bw, bh) = self.shape.bounds();
        let w = bw * self.scale;
        let h = bh * self.scale;
        Rect::new(self.pos.x - w / 2.0, self.pos.y - h / 2.0, w, h)
    }

    /// Pixel rects approximating this pet's silhouette, in canvas coordinates, for
    /// building a click-capturing input region (the rest of the window is
    /// click-through). A [`PetShape::Circle`] is approximated by 1px-tall
    /// horizontal scanlines → a true circular hit area (not just its bounding
    /// box); a [`PetShape::Rect`] is itself. Callers (the platform backend) union
    /// these into a compositor input region each frame as the pet moves.
    pub fn region_rects(&self) -> Vec<Rect> {
        let (bw, bh) = self.shape.bounds();
        let w = bw * self.scale;
        let h = bh * self.scale;
        let (cx, cy) = (self.pos.x, self.pos.y);
        match self.shape {
            PetShape::Circle { .. } => {
                let r = w / 2.0;
                if r <= 0.0 {
                    return Vec::new();
                }
                let y0 = (cy - r).floor();
                let y1 = (cy + r).ceil();
                let mut rects = Vec::with_capacity((y1 - y0).max(0.0) as usize + 1);
                let mut y = y0;
                while y < y1 {
                    // sample at the scanline midpoint for a symmetric circle
                    let dy = (y + 0.5) - cy;
                    let half_sq = r * r - dy * dy;
                    if half_sq > 0.0 {
                        let half = half_sq.sqrt();
                        rects.push(Rect::new(cx - half, y, half * 2.0, 1.0));
                    }
                    y += 1.0;
                }
                rects
            }
            PetShape::Rect { .. } => vec![Rect::new(cx - w / 2.0, cy - h / 2.0, w, h)],
        }
    }
}

/// The whole drawable scene for one frame.
#[derive(Clone, Debug)]
pub struct Scene {
    pub pet: Pet,
    /// Logical canvas size in pixels; pet.pos is relative to this.
    pub canvas_size: (u32, u32),
    /// Background fill. Transparent in production; opaque for headless tests.
    pub bg: Color,
}

impl Scene {
    pub fn demo(canvas_size: (u32, u32)) -> Self {
        let center = Vec2::new(canvas_size.0 as f32 / 2.0, canvas_size.1 as f32 / 2.0);
        Self {
            pet: Pet {
                pos: center,
                shape: PetShape::Circle { radius: 64.0 },
                color: Color::CORAL,
                scale: 1.0,
            },
            canvas_size,
            bg: Color::TRANSPARENT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_region_is_irregular_not_just_bbox() {
        // Circle r=50 centered at (100,100): the click region should be the disc,
        // so the bounding-box corners (e.g. (50,50)) are click-through.
        let pet = Pet {
            pos: Vec2::new(100.0, 100.0),
            shape: PetShape::Circle { radius: 50.0 },
            color: Color::CORAL,
            scale: 1.0,
        };
        let rects = pet.region_rects();
        assert!(!rects.is_empty(), "scanlines should cover the disc");
        // widest scanline ~= diameter (the disc's middle), not more.
        let widest = rects.iter().map(|r| r.w).fold(0.0_f32, f32::max);
        assert!(widest > 90.0 && widest <= 100.0, "widest scanline ~ diameter, got {widest}");
        // the bbox corner (50,50) is outside the circle → no scanline covers it.
        let corner_covered = rects.iter().any(|r| {
            r.x <= 50.0 && 50.0 < r.x + r.w && r.y <= 50.0 && 50.0 < r.y + r.h
        });
        assert!(!corner_covered, "bbox corner must be click-through");
    }

    #[test]
    fn rect_region_is_the_bbox() {
        let pet = Pet {
            pos: Vec2::new(100.0, 100.0),
            shape: PetShape::Rect { w: 40.0, h: 60.0 },
            color: Color::CORAL,
            scale: 1.0,
        };
        let rects = pet.region_rects();
        assert_eq!(rects, vec![Rect::new(80.0, 70.0, 40.0, 60.0)]);
    }

    #[test]
    fn region_scales_with_pet_scale() {
        let mut pet = Pet {
            pos: Vec2::new(100.0, 100.0),
            shape: PetShape::Circle { radius: 50.0 },
            color: Color::CORAL,
            scale: 1.0,
        };
        let r1 = pet.region_rects();
        pet.scale = 2.0;
        let r2 = pet.region_rects();
        let widest = |r: &Vec<Rect>| r.iter().map(|x| x.w).fold(0.0_f32, f32::max);
        assert!(widest(&r2) > widest(&r1), "scaled pet has a wider region");
    }
}
