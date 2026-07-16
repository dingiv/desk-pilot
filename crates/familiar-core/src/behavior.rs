//! Behavior state machine. Advances the scene over time using canvas-space
//! coordinates — per docs/index.md §3 we move the pet *inside* the canvas,
//! never the window.

use crate::{geometry::Vec2, Scene};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsmState {
    Idle,
    Walk,
    Sleep,
    Drag,
    React,
}

/// Tiny FSM: idle bob + slow drift; drag overrides position to follow pointer.
/// This is the skeleton — richer behaviors land in M2 (see docs/behavior.md).
pub struct Fsm {
    pub state: FsmState,
    phase: f32,
    vel: Vec2,
    drag_offset: Vec2,
}

impl Default for Fsm {
    fn default() -> Self {
        Self::new()
    }
}

impl Fsm {
    pub fn new() -> Self {
        Self {
            state: FsmState::Idle,
            phase: 0.0,
            vel: Vec2::new(20.0, 0.0),
            drag_offset: Vec2::new(0.0, 0.0),
        }
    }

    /// Pointer pressed (only call when a hit-test already passed).
    pub fn on_pointer_down(&mut self, scene: &mut Scene, at: Vec2) {
        self.state = FsmState::Drag;
        self.drag_offset = Vec2::new(scene.pet.pos.x - at.x, scene.pet.pos.y - at.y);
    }

    pub fn on_pointer_move(&mut self, scene: &mut Scene, at: Vec2) {
        if self.state == FsmState::Drag {
            scene.pet.pos = Vec2::new(at.x + self.drag_offset.x, at.y + self.drag_offset.y);
        }
    }

    pub fn on_pointer_up(&mut self) {
        if self.state == FsmState::Drag {
            self.state = FsmState::Idle;
        }
    }

    /// Advance one frame. `dt` in seconds.
    pub fn step(&mut self, dt: f32, scene: &mut Scene) {
        self.phase += dt;
        let (w, h) = (scene.canvas_size.0 as f32, scene.canvas_size.1 as f32);

        if self.state == FsmState::Drag {
            return;
        }

        // gentle horizontal drift, bouncing off canvas edges
        let mut p = scene.pet.pos;
        p.x += self.vel.x * dt;
        let half = scene.pet.bounding_rect().w / 2.0;
        if p.x < half {
            p.x = half;
            self.vel.x = self.vel.x.abs();
        } else if p.x > w - half {
            p.x = w - half;
            self.vel.x = -self.vel.x.abs();
        }

        // idle vertical bob
        p.y += self.phase.sin() * 0.15;
        let half_h = scene.pet.bounding_rect().h / 2.0;
        if p.y < half_h {
            p.y = half_h;
        } else if p.y > h - half_h {
            p.y = h - half_h;
        }
        scene.pet.pos = p;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Vec2;
    use crate::scene::Scene;

    fn scene_at(x: f32, y: f32) -> Scene {
        let mut s = Scene::demo((200, 200));
        s.pet.pos = Vec2::new(x, y);
        s
    }

    #[test]
    fn drag_follows_pointer_preserving_grab_offset() {
        let mut s = scene_at(100.0, 100.0);
        let mut fsm = Fsm::new();
        // grab 10px left of center: the pet should stay glued to the cursor,
        // i.e. move by the same delta the cursor moves.
        fsm.on_pointer_down(&mut s, Vec2::new(90.0, 100.0));
        assert_eq!(fsm.state, FsmState::Drag);
        // cursor moves +20x, -10y → pet moves the same (+20, -10) → (120, 90)
        fsm.on_pointer_move(&mut s, Vec2::new(110.0, 90.0));
        assert_eq!(s.pet.pos, Vec2::new(120.0, 90.0));
        // release → idle, pet stays where it was dropped
        fsm.on_pointer_up();
        assert_eq!(fsm.state, FsmState::Idle);
        assert_eq!(s.pet.pos, Vec2::new(120.0, 90.0));
    }

    #[test]
    fn drag_suppresses_auto_drift() {
        let mut s = scene_at(100.0, 100.0);
        let mut fsm = Fsm::new();
        fsm.on_pointer_down(&mut s, Vec2::new(100.0, 100.0));
        let before = s.pet.pos;
        // a step while dragging must not drift/bob the pet
        fsm.step(0.1, &mut s);
        assert_eq!(s.pet.pos, before, "no auto-drift while dragging");
    }

    #[test]
    fn no_drag_until_pointer_down() {
        let mut s = scene_at(100.0, 100.0);
        let mut fsm = Fsm::new();
        // pointer move without a press must not drag
        fsm.on_pointer_move(&mut s, Vec2::new(50.0, 50.0));
        assert_eq!(s.pet.pos, Vec2::new(100.0, 100.0), "move without press is a no-op");
    }
}
