//! Pure-Rust core for geek-familiar.
//!
//! No platform or rendering dependencies — compiles everywhere. Holds the scene
//! model, geometry primitives, the behavior state machine, runtime config, and
//! the CPU canvas (RGBA8 framebuffer) that renderers paint into and platforms
//! blit to screen.

pub mod behavior;
pub mod canvas;
pub mod config;
pub mod geometry;
pub mod scene;

pub use behavior::{Fsm, FsmState};
pub use canvas::Canvas;
pub use config::Config;
pub use geometry::{Color, Rect, Vec2};
pub use scene::{Pet, PetShape, Scene};
