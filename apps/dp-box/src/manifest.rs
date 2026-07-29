use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct AppManifest {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub build: BuildConfig,
    pub exec: ExecConfig,
    #[serde(default)]
    pub config: Option<ConfigRef>,
    #[serde(default)]
    pub system_deps: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct BuildConfig {
    pub system: String,
    pub package: String,
    #[serde(rename = "binary")]
    pub binary_name: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default = "default_release")]
    pub default_profile: String,
}

fn default_release() -> String { "release".into() }

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ExecConfig {
    pub binary_relative: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ConfigRef {
    pub file_loader_ns: String,
    #[serde(default)]
    pub default_config: Option<serde_yaml::Value>,
}

pub fn load_manifest(name: &str) -> Option<AppManifest> {
    let loader = fs::loader!();
    let yaml = loader.read_str(&format!("MANIFESTS::{name}.yaml")).ok()?;
    serde_yaml::from_str::<AppManifest>(&yaml).ok()
}

pub fn detect_workspace_root() -> PathBuf {
    if fs::is_dev() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut candidate: &std::path::Path = &manifest_dir;
        loop {
            let cargo_toml = candidate.join("Cargo.toml");
            if cargo_toml.exists() {
                if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
                    if content.contains("[workspace]") {
                        return candidate.to_path_buf();
                    }
                }
            }
            candidate = match candidate.parent() {
                Some(p) => p,
                None => break,
            };
        }
        manifest_dir.parent().map(|p| p.to_path_buf()).unwrap_or(manifest_dir)
    } else {
        expand_tilde("~/.desk-pilot")
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}
