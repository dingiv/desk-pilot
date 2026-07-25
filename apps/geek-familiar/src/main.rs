use geek_familiar::PetApp;
use platform::PlatformBackend;

/// Runtime config parsed from `familiar.yaml` (FileLoader CONF namespace).
#[derive(serde::Deserialize)]
#[serde(default)]
struct FamiliarConfig {
    /// audio-aura daemon SSE address.
    aura_addr: String,
    /// Pet skin asset (resolved via ui::skin_source).
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

fn main() {
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

    let app = PetApp::with_config(&cfg.aura_addr, &cfg.skin);

    #[cfg(feature = "gtk")]
    {
        let mut backend = platform::gtk::GtkBackend::new();
        backend.run(Box::new(app));
    }

    #[cfg(not(feature = "gtk"))]
    {
        let mut backend = platform::HeadlessBackend::default();
        backend.run(Box::new(app));
    }
}
