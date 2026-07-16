//! Wires the core FSM + CPU renderer into a runnable `App`, and declares the
//! pet's UI (egui path) as a pure `ui::View` tree.

use std::time::Duration;

use core::{geometry::Vec2, Canvas, Color, Config, Fsm, Scene};
use platform::{App, InputRegion, PlatformEvent};
use render::{CpuRenderer, Renderer};
use ui::{column, text, text_edit, button, image_src, Id, ImageSource, Msg, View};

/// Widget ids (the app maps these to semantics in [`PetApp::update`]).
const ID_MSG: Id = 1;
const ID_SEND: Id = 2;

pub struct PetApp {
    scene: Scene,
    fsm: Fsm,
    renderer: CpuRenderer,
    /// The pet's body image, resolved once at startup (FileLoader → bundled fallback).
    skin: ImageSource,
    // egui-path UI state (business logic; declaration lives in `view`)
    msg: String,
    clicks: u32,
}

impl PetApp {
    pub fn from_config(cfg: &Config) -> Self {
        let mut scene = Scene::demo(cfg.canvas_size);
        scene.bg = cfg.bg;
        scene.pet.color = cfg.pet_color;
        Self {
            scene,
            fsm: Fsm::new(),
            renderer: CpuRenderer::new(),
            skin: ui::skin_source("default/idle.png"),
            msg: String::from("hello pet"),
            clicks: 0,
        }
    }

    pub fn demo() -> Self {
        Self::from_config(&Config::default())
    }
}

impl App for PetApp {
    fn canvas_size(&self) -> (u32, u32) {
        self.scene.canvas_size
    }

    fn input_region(&self) -> InputRegion {
        // The pet's silhouette (circle → scanline rects) captures clicks; the
        // rest of the window is click-through. Tracks the pet each frame.
        InputRegion::from_rects(self.scene.pet.region_rects())
    }

    fn handle_event(&mut self, ev: &PlatformEvent) {
        match *ev {
            PlatformEvent::PointerDown { pos, .. } => {
                if hit_pet(&self.scene, pos) {
                    self.fsm.on_pointer_down(&mut self.scene, pos);
                }
            }
            PlatformEvent::PointerMove { pos } => self.fsm.on_pointer_move(&mut self.scene, pos),
            PlatformEvent::PointerUp { .. } => self.fsm.on_pointer_up(),
            PlatformEvent::Resize { width, height } => {
                self.scene.canvas_size = (width, height);
            }
            PlatformEvent::Close => {}
        }
    }

    fn tick(&mut self, dt: Duration) {
        self.fsm.step(dt.as_secs_f32(), &mut self.scene);
    }

    fn render(&self, out: &mut Canvas) {
        self.renderer.render(&self.scene, out);
    }

    /// Declare the pet UI as a pure `View` tree (Flutter/Elm-style). The
    /// platform renders this; interactions come back via [`App::update`].
    fn view(&self) -> View {
        // The pet body is the skin's idle PNG (irregular shape, transparent
        // surround), resolved at startup via FileLoader (dev: crates/ui/assets/
        // skins/, prod: ~/.geek-familiar/skins/; bundled fallback) and rendered
        // via the crisp Lanczos path. Click-through is derived from its
        // rendered alpha — the window is the pet's silhouette.
        column(vec![
            image_src(self.skin.clone(), 200.0, 200.0),
            text("Pet Secretary").color(Color::WHITE).size(18.0),
            text_edit(ID_MSG, &self.msg),
            button(format!("Send ({})", self.clicks), ID_SEND),
        ])
    }

    /// React to UI interactions (the "update" half of the Elm model).
    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Clicked(ID_SEND) => self.clicks += 1,
            Msg::TextChanged(ID_MSG, s) => self.msg = s,
            _ => {}
        }
    }
}

fn hit_pet(scene: &Scene, p: Vec2) -> bool {
    let r = scene.pet.bounding_rect();
    p.x >= r.x && p.x <= r.x + r.w && p.y >= r.y && p.y <= r.y + r.h
}
