//! geek-familiar entry — loads `familiar.yaml` (FileLoader), resolves the skin,
//! and runs the pet as an iced application (transparent + borderless window).

mod model;
mod service;
mod view;
mod input_region;
mod app;

use app::{skin_source, DockingPreference, PetApp, StyleConfig};
use iced::window;

/// Runtime config parsed from `familiar.yaml` (FileLoader CONF namespace).
#[derive(Debug, serde::Deserialize)]
#[serde(default)]
struct FamiliarConfig {
    /// audio-aura daemon SSE address.
    aura_addr: String,
    /// Pet skin asset (resolved via [`geek_familiar::skin_source`]).
    skin: String,
    /// Base font size in logical pixels. Dock labels, panel text, and the IME
    /// input are all derived from this (≈ 0.85× for smaller labels, 1.0× for body).
    #[serde(default = "default_font_size")]
    font_size: f32,
    /// Style overrides (colours, including the window background). See the
    /// `style:` section in familiar.yaml.
    #[serde(default)]
    style: StyleConfig,
    /// Window background colour. Format: `"r, g, b, a"` (comma-separated 0..1).
    /// Omit or delete for fully transparent (default).
    #[serde(default)]
    window_bg: Option<String>,
    /// Which side of the desktop the pet docks to.  "left" (default) or "right".
    /// Mirrors UI alignment and the resize-grip corner.
    #[serde(default)]
    docking_preference: DockingPreference,
    /// Pet sprite display size (square, in logical pixels).
    #[serde(default = "default_sprite_size")]
    sprite_size: f32,
    /// Sprite filter: "linear" (smooth) or "nearest" (crisp pixel art).
    #[serde(default = "default_sprite_filter")]
    sprite_filter: String,
}

fn default_sprite_size() -> f32 { 180.0 }
fn default_sprite_filter() -> String { "linear".into() }

fn default_font_size() -> f32 { 14.0 }

impl Default for FamiliarConfig {
    fn default() -> Self {
        Self {
            aura_addr: "127.0.0.1:9091".into(),
            skin: "default/idle.png".into(),
            font_size: default_font_size(),
            style: StyleConfig::default(),
            window_bg: None,
            docking_preference: DockingPreference::default(),
            sprite_size: default_sprite_size(),
            sprite_filter: default_sprite_filter(),
        }
    }
}

pub fn main() -> iced::Result {
    // Load familiar.yaml via FileLoader (dev: this crate's dir, prod: ~/.desk-pilot/).
    let fs = fs::loader!();
    let cfg: FamiliarConfig = match fs.read_str("CONF::familiar.yaml") {
        Ok(text) => {
            let cfg: FamiliarConfig = serde_yaml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("[geek-familiar] config parse error ({e}), using defaults");
                FamiliarConfig::default()
            });
            eprintln!("[geek-familiar] config loaded: aura={} skin={} font={} window_bg={:?}", cfg.aura_addr, cfg.skin, cfg.font_size, cfg.window_bg);
            cfg
        }
        Err(e) => {
            eprintln!("[geek-familiar] no familiar.yaml ({e}), using defaults");
            FamiliarConfig::default()
        }
    };

    let skin = skin_source(&cfg.skin);
    let aura_addr = cfg.aura_addr;
    let font_size = cfg.font_size;
    let style = cfg.style;
    let window_bg = cfg.window_bg;
    let docking = cfg.docking_preference;
    let sprite_size = cfg.sprite_size;
    let sprite_filter = cfg.sprite_filter;
    let token = format!("geek-familiar-{}", std::process::id());

    iced::application(
        move || PetApp::new(aura_addr.clone(), skin.clone(), token.clone(), font_size, style.clone(), window_bg.clone(), docking, sprite_size, sprite_filter.clone()),
        PetApp::update,
        PetApp::view,
    )
    .title(PetApp::title)
    .theme(PetApp::theme)
    .subscription(PetApp::subscription)
    .window(window::Settings {
        transparent: true,
        decorations: false,
        resizable: true,
        blur: true,  // native frosted-glass (Wayland may ignore; no-op if unsupported)
        size: iced::Size::new(260.0, 380.0),
        // Wayland ignores AlwaysOnTop (winit no-op); gnome-layer-ext handles it.
        level: window::Level::AlwaysOnTop,
        exit_on_close_request: true,
        ..Default::default()
    })
    .run()
}
