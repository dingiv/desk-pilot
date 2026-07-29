use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerConfig {
    /// Window width in pixels. None = window manager decides.
    #[serde(default)]
    pub width: Option<u32>,
    /// Window height in pixels. None = window manager decides.
    #[serde(default)]
    pub height: Option<u32>,
    /// Workspace root override (auto-detected if null).
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// Default cargo build profile.
    #[serde(default = "default_build_profile")]
    pub build_profile: String,
}

fn default_build_profile() -> String {
    "release".into()
}

impl ManagerConfig {
    /// Load config from the CONF namespace. Returns default if file is missing.
    pub fn load() -> Self {
        let loader = fs::loader!();
        match loader.read_str("CONF::manager.yaml") {
            Ok(yaml) => serde_yaml::from_str(&yaml).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist current config back to the CONF namespace.
    pub fn save(&self) {
        let loader = fs::loader!();
        if let Ok(yaml) = serde_yaml::to_string(self) {
            let _ = loader.write_str("CONF::manager.yaml", &yaml);
        }
    }
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            workspace_root: None,
            build_profile: "release".into(),
        }
    }
}
