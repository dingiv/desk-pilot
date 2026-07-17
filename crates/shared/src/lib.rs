//! shared — unified asset + data file resolution for cargo crates.
//!
//! ## Namespaces from Cargo.toml (zero-config in calling code)
//!
//! Declare namespaces in the crate's `Cargo.toml`:
//! ```toml
//! [package.metadata.shared]
//! CONF_DIR  = { dev = "data/conf",  prod = "~/.my-app/conf" }
//! MODEL_DIR = { dev = "data/model", prod = "~/.my-app/model" }
//! ```
//!
//! Add a one-line `build.rs`:
//! ```rust
//! fn main() { shared::emit_namespaces(); }
//! ```
//!
//! In code, `loader!()` auto-discovers all declared namespaces — the caller never sees paths:
//! ```ignore
//! let fs = loader!();                              // auto-loaded from Cargo.toml
//! let cfg  = fs.read_str("CONF_DIR::my.conf")?;    // dev→data/conf/  prod→~/.my-app/conf/
//! let logo = fs.read("logo.png")?;                 // bare = asset: dev→assets/  prod→exe/assets/
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// ── Namespace ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Namespace {
    pub dev: String,
    pub prod: String,
}

// ── FileLoader ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileLoader {
    manifest_dir: PathBuf,
    assets_subdir: String,
    namespaces: HashMap<String, Namespace>,
}

impl FileLoader {
    pub fn new(manifest_dir: impl Into<PathBuf>, assets_subdir: impl Into<String>) -> Self {
        Self {
            manifest_dir: manifest_dir.into(),
            assets_subdir: assets_subdir.into(),
            namespaces: HashMap::new(),
        }
    }

    /// Register a namespace manually (when NOT using Cargo.toml + build.rs).
    pub fn namespace(
        mut self,
        name: &str,
        dev: impl Into<String>,
        prod: impl Into<String>,
    ) -> Self {
        self.namespaces
            .insert(name.to_string(), Namespace { dev: dev.into(), prod: prod.into() });
        self
    }

    /// Resolve `"path"` or `"NS::path"`.
    pub fn resolve(&self, path_or_ns: &str) -> Option<PathBuf> {
        if let Some((ns, rel)) = path_or_ns.split_once("::") {
            return self.resolve_ns(ns, rel);
        }
        self.candidates(path_or_ns).into_iter().find(|p| p.exists())
    }

    fn resolve_ns(&self, ns: &str, rel: &str) -> Option<PathBuf> {
        let cfg = self.namespaces.get(ns)?;
        let root = if is_dev() {
            self.manifest_dir.join(&cfg.dev)
        } else {
            expand_tilde(&cfg.prod)
        };
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Some(path)
    }

    pub fn candidates(&self, rel: &str) -> Vec<PathBuf> {
        let mut v = Vec::with_capacity(3);
        v.push(self.manifest_dir.join(&self.assets_subdir).join(rel));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                v.push(exe_dir.join(&self.assets_subdir).join(rel));
            }
        }
        v.push(Path::new(&self.assets_subdir).join(rel));
        v
    }

    pub fn read(&self, path_or_ns: &str) -> Result<Vec<u8>> {
        let path = self.resolve(path_or_ns).ok_or_else(|| {
            let tried = self
                .candidates(path_or_ns)
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::anyhow!("file not found: {path_or_ns}\ntried:\n{tried}")
        })?;
        std::fs::read(&path).with_context(|| format!("read {}", path.display()))
    }

    pub fn read_str(&self, path_or_ns: &str) -> Result<String> {
        let bytes = self.read(path_or_ns)?;
        String::from_utf8(bytes).context("file is not valid UTF-8")
    }

    pub fn write(&self, path_or_ns: &str, data: &[u8]) -> Result<()> {
        let path = match self.resolve(path_or_ns) {
            Some(p) => p,
            None => self
                .candidates(path_or_ns)
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("no candidate for {path_or_ns}"))?,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent {}", parent.display()))?;
        }
        std::fs::write(&path, data).with_context(|| format!("write {}", path.display()))
    }

    pub fn write_str(&self, path_or_ns: &str, contents: &str) -> Result<()> {
        self.write(path_or_ns, contents.as_bytes())
    }

    pub fn exists(&self, path_or_ns: &str) -> bool {
        self.resolve(path_or_ns).is_some()
    }
}

impl Default for FileLoader {
    fn default() -> Self {
        Self::new(env!("CARGO_MANIFEST_DIR"), "assets")
    }
}

/// Build a [`FileLoader`] for the **calling crate's** `assets/`, auto-loading namespaces
/// declared in `[package.metadata.shared]` (requires the crate to have a `build.rs` that calls
/// [`emit_namespaces`]). For bare-path assets only (no namespaces), it works without build.rs
/// (the generated file defaults to an empty table).
///
/// The calling code NEVER sees path values — only namespace names (`"CONF_DIR::my.conf"`).
#[macro_export]
macro_rules! loader {
    () => {{
        const __NS: &[(&str, &str, &str)] =
            include!(concat!(env!("OUT_DIR"), "/shared_ns.rs"));
        let mut __l = $crate::FileLoader::new(env!("CARGO_MANIFEST_DIR"), "assets");
        for &(__n, __d, __p) in __NS {
            __l = __l.namespace(__n, __d, __p);
        }
        __l
    }};
    ($sub:literal) => {{
        const __NS: &[(&str, &str, &str)] =
            include!(concat!(env!("OUT_DIR"), "/shared_ns.rs"));
        let mut __l = $crate::FileLoader::new(env!("CARGO_MANIFEST_DIR"), $sub);
        for &(__n, __d, __p) in __NS {
            __l = __l.namespace(__n, __d, __p);
        }
        __l
    }};
}

// ── build.rs helper: read Cargo.toml namespaces → generate shared_ns.rs ──────

/// Call from a crate's `build.rs`: reads `[package.metadata.shared]` from this crate's
/// `Cargo.toml`, generates `{OUT_DIR}/shared_ns.rs` (a const table consumed by [`loader!`]).
///
/// If no namespaces are declared, generates an empty table (so `loader!()` still compiles).
///
/// ```ignore
/// // build.rs
/// fn main() { shared::emit_namespaces(); }
/// ```
pub fn emit_namespaces() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR not set (must be called from build.rs)");
    let out_dir =
        std::env::var("OUT_DIR").expect("OUT_DIR not set (must be called from build.rs)");
    let cargo_toml = Path::new(&manifest_dir).join("Cargo.toml");
    let namespaces = parse_metadata(&cargo_toml);
    // Generate the Rust source.
    let mut code = String::from("// AUTO-GENERATED by shared::emit_namespaces() — do not edit.\n&[\n");
    for (name, dev, prod) in &namespaces {
        code.push_str(&format!(
            "    (\"{}\", \"{}\", \"{}\"),\n",
            escape(name),
            escape(dev),
            escape(prod)
        ));
    }
    code.push_str("]\n");
    let dest = Path::new(&out_dir).join("shared_ns.rs");
    std::fs::write(&dest, code)
        .unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
    println!("cargo:rerun-if-changed=Cargo.toml");
}

/// Parse `[package.metadata.shared]` from Cargo.toml. Returns `Vec<(name, dev, prod)>`.
/// Naive scanner — handles the inline-table format:
/// ```toml
/// [package.metadata.shared]
/// CONF_DIR = { dev = "data/conf", prod = "~/.my-app/conf" }
/// ```
fn parse_metadata(cargo_toml: &Path) -> Vec<(String, String, String)> {
    let Ok(content) = std::fs::read_to_string(cargo_toml) else {
        return Vec::new();
    };
    // Find the [package.metadata.shared] section.
    let marker = "[package.metadata.shared]";
    let start = match content.find(marker) {
        Some(i) => i + marker.len(),
        None => return Vec::new(), // no namespace section — empty table
    };
    // Lines until the next `[...]` section header.
    let section: Vec<&str> = content[start..]
        .lines()
        .take_while(|l| !l.trim_start().starts_with('['))
        .collect();
    // Parse lines like:  KEY = { dev = "...", prod = "..." }
    let mut result = Vec::new();
    for line in section {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, val)) = parse_inline_table(line) {
            result.push((name, val.0, val.1));
        }
    }
    result
}

/// Parse `KEY = { dev = "a", prod = "b" }` → `(KEY, (dev, prod))`.
fn parse_inline_table(line: &str) -> Option<(String, (String, String))> {
    let (key, rest) = line.split_once('=')?;
    let name = key.trim().to_string();
    // Extract quoted values for dev and prod from the inline table.
    let dev = extract_quoted(rest, "dev")?;
    let prod = extract_quoted(rest, "prod")?;
    Some((name, (dev, prod)))
}

/// Find `key = "value"` within `hay` and return the value.
fn extract_quoted(hay: &str, key: &str) -> Option<String> {
    let needle = format!("{key} = \"");
    let start = hay.find(&needle)? + needle.len();
    let rest = &hay[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ── dev/prod detection + data_dir ──────────────────────────────────────────────

pub fn is_dev() -> bool {
    std::env::var("CARGO_MANIFEST_DIR").is_ok()
}

pub fn data_dir(app_key: &str) -> Result<PathBuf> {
    let env_key = format!("{}_DATA_DIR", app_key.to_uppercase().replace('-', "_"));
    if let Ok(d) = std::env::var(&env_key) {
        let p = PathBuf::from(d);
        std::fs::create_dir_all(&p)
            .with_context(|| format!("create data dir {} (from {env_key})", p.display()))?;
        return Ok(p);
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let p = PathBuf::from(manifest).join("data");
        std::fs::create_dir_all(&p)?;
        return Ok(p);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .map_err(|_| anyhow::anyhow!("neither XDG_DATA_HOME nor HOME set"))?;
    let p = base.join(app_key);
    std::fs::create_dir_all(&p)?;
    Ok(p)
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

// ── shared's own build.rs (generates its own empty namespace table) ──────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_asset_resolves_in_source_tree() {
        let loader = FileLoader::new(env!("CARGO_MANIFEST_DIR"), "src");
        assert!(loader.exists("lib.rs"));
    }

    #[test]
    fn bare_asset_not_found_lists_candidates() {
        let loader = FileLoader::new(env!("CARGO_MANIFEST_DIR"), "assets");
        let err = loader.read_str("__nope__.xyz").unwrap_err();
        assert!(format!("{err}").contains("tried:"));
    }

    #[test]
    fn namespace_resolves() {
        let loader = FileLoader::new(env!("CARGO_MANIFEST_DIR"), "assets")
            .namespace("CONF", "data/conf", "~/.my-app/conf");
        let path = loader.resolve("CONF::x.toml").unwrap();
        assert!(path.ends_with("data/conf/x.toml"));
    }

    #[test]
    fn namespace_unknown_returns_none() {
        let loader = FileLoader::new(env!("CARGO_MANIFEST_DIR"), "assets");
        assert!(loader.resolve("NOPE::x").is_none());
    }

    #[test]
    fn namespace_write_and_read_back() {
        let loader = FileLoader::new(env!("CARGO_MANIFEST_DIR"), "assets")
            .namespace("OUT", "data/test_output", "~/.my-app/out");
        loader.write("OUT::roundtrip.txt", b"hello").unwrap();
        assert_eq!(loader.read_str("OUT::roundtrip.txt").unwrap(), "hello");
        let _ = std::fs::remove_file(loader.resolve("OUT::roundtrip.txt").unwrap());
    }

    #[test]
    fn extract_quoted_finds_value() {
        assert_eq!(
            extract_quoted(r#"{ dev = "a/b", prod = "~/c" }"#, "dev"),
            Some("a/b".into())
        );
        assert_eq!(
            extract_quoted(r#"{ dev = "a/b", prod = "~/c" }"#, "prod"),
            Some("~/c".into())
        );
    }

    #[test]
    fn parse_inline_table_works() {
        let (name, (dev, prod)) =
            parse_inline_table(r#"CONF_DIR = { dev = "data/conf", prod = "~/.my-app/conf" }"#)
                .unwrap();
        assert_eq!(name, "CONF_DIR");
        assert_eq!(dev, "data/conf");
        assert_eq!(prod, "~/.my-app/conf");
    }

    #[test]
    fn parse_metadata_no_section() {
        // A Cargo.toml without [package.metadata.shared] → empty vec.
        let v = parse_metadata(Path::new("/dev/null"));
        assert!(v.is_empty());
    }

    #[test]
    fn expand_tilde_works() {
        std::env::set_var("HOME", "/tmp/fake_home");
        assert_eq!(expand_tilde("~/foo"), PathBuf::from("/tmp/fake_home/foo"));
        assert_eq!(expand_tilde("~"), PathBuf::from("/tmp/fake_home"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn is_dev_true_under_cargo() {
        assert!(is_dev());
    }
}

// ── logging: process-wide tracing subscriber init (binaries only; `log` feature) ──

/// Initialize the process-wide `tracing` subscriber. Call ONCE at the very top of a binary's
/// `main` (init-stage side effect; lib crates only use the `tracing` facade macros).
///
/// - **Dev builds** (`debug_assertions`): human-readable colored output.
/// - **Release builds**: one JSON object per line — machine-parseable for ELK/Loki.
/// - **Level control**: the standard `RUST_LOG` env filter (e.g. `RUST_LOG=debug`,
///   `RUST_LOG=aura_daemon=trace,info`); defaults to `info` when unset.
/// - **Writer**: stderr, always. Log rotation/shipping is the HOST's job
///   (journald / logrotate / docker log-driver) — the process never writes log files itself.
#[cfg(feature = "log")]
pub fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    if cfg!(debug_assertions) {
        registry.with(fmt::layer().with_writer(std::io::stderr)).init();
    } else {
        registry.with(fmt::layer().json().with_writer(std::io::stderr)).init();
    }
}
