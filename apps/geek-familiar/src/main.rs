//! geek-familiar entry — loads `familiar.yaml` (FileLoader), resolves the skin,
//! and runs the pet as an iced application (transparent + borderless window).

use geek_familiar::{skin_source, PetApp};
use iced::window;

/// Runtime config parsed from `familiar.yaml` (FileLoader CONF namespace).
#[derive(serde::Deserialize)]
#[serde(default)]
struct FamiliarConfig {
    /// audio-aura daemon SSE address.
    aura_addr: String,
    /// Pet skin asset (resolved via [`geek_familiar::skin_source`]).
    skin: String,
}

impl Default for FamiliarConfig {
    fn default() -> Self {
        Self {
            aura_addr: "127.0.0.1:9091".into(),
            skin: "default/idle.png".into(),
        }
    }
}

pub fn main() -> iced::Result {
    // Load familiar.yaml via FileLoader (dev: this crate's dir, prod: ~/.desk-pilot/).
    let fs = fs::loader!();
    let cfg: FamiliarConfig = match fs.read_str("CONF::familiar.yaml") {
        Ok(text) => serde_yaml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("[geek-familiar] config parse error ({e}), using defaults");
            FamiliarConfig::default()
        }),
        Err(e) => {
            eprintln!("[geek-familiar] no familiar.yaml ({e}), using defaults");
            FamiliarConfig::default()
        }
    };

    let skin = skin_source(&cfg.skin);
    let aura_addr = cfg.aura_addr;
    let token = format!("geek-familiar-{}", std::process::id());

    iced::application(
        move || PetApp::new(aura_addr.clone(), skin.clone(), token.clone()),
        PetApp::update,
        PetApp::view,
    )
    .title(PetApp::title)
    .theme(PetApp::theme)
    .subscription(PetApp::subscription)
    .window(window::Settings {
        transparent: true,
        decorations: false,
        resizable: false,
        size: iced::Size::new(260.0, 380.0),
        // Wayland ignores AlwaysOnTop (winit no-op); gnome-layer-ext handles it.
        level: window::Level::AlwaysOnTop,
        exit_on_close_request: true,
        ..Default::default()
    })
    .run()
}
