//! Runtime configuration (defaults only for now; load/save comes later).

use crate::geometry::Color;

#[derive(Clone, Debug)]
pub struct Config {
    pub canvas_size: (u32, u32),
    pub bg: Color,
    pub pet_color: Color,
    pub auto_move: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            canvas_size: (320, 240),
            bg: Color::TRANSPARENT,
            pet_color: Color::CORAL,
            auto_move: true,
        }
    }
}
