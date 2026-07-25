//! Declarative UI layer — a renderer-agnostic [`View`] tree + [`Msg`] events.
//!
//! Business logic declares *what* the UI looks like (a [`View`] tree, built
//! fresh each frame from state, Flutter/Elm-style); a renderer (the egui binder
//! in `familiar-render`) walks the tree to paint it and reports interactions
//! back as [`Msg`]s. This keeps UI declaration (frequently-changing business
//! logic, and later user-customizable appearance) separate from the render layer.
//!
//! `View` is pure data (no `&mut`, no renderer types); `Msg` is the only
//! feedback channel. `Id` lets the app map a widget back to a semantic action.

// UI component constructors use PascalCase to match Flutter ergonomics
// (Text("hi"), Column![...], Button("ok", id)). Rust convention is snake_case,
// but this is a deliberate DSL choice — verified: cross-crate calls don't warn.
#![allow(non_snake_case)]

pub use core::Color;

/// Bundled pet assets (embedded at compile time via `include_bytes!`).
pub mod assets {
    /// The default idle form — a high-res irregular-shape PNG with transparent surround.
    pub const IDLE_PNG: &[u8] = include_bytes!("../assets/skins/default/idle.png");
}

/// Resolve a skin file (`"<skin>/<file>"`, e.g. `"default/idle.png"`) to an
/// [`ImageSource`] via the `SKIN` namespace (declared in this crate's
/// `Cargo.toml`): dev → `assets/skins/`, prod → `~/.geek-familiar/skins/`.
/// Falls back to the bundled [`assets::IDLE_PNG`] when the file is missing.
/// Resolve once at startup, not per frame.
pub fn skin_source(rel: &str) -> ImageSource {
    let loader = fs::loader!();
    match loader.resolve(&format!("SKIN::{rel}")).filter(|p| p.exists()) {
        Some(p) => {
            eprintln!("[geek-familiar] skin: {}", p.display());
            ImageSource::Path(p.to_string_lossy().into_owned())
        }
        None => {
            eprintln!("[geek-familiar] skin: {rel} not found, using bundled fallback");
            ImageSource::Bytes(assets::IDLE_PNG)
        }
    }
}

/// How a [`View::Image`] references its asset.
#[derive(Clone, Debug)]
pub enum ImageSource {
    Path(String),
    Bytes(&'static [u8]),
}

impl ImageSource {
    pub fn cache_key(&self) -> String {
        match self {
            ImageSource::Path(p) => p.clone(),
            ImageSource::Bytes(b) => format!("bytes:{:p}", b.as_ptr()),
        }
    }
}

pub type Id = u64;

#[derive(Clone, Debug)]
pub enum Msg {
    Clicked(Id),
    TextChanged(Id, String),
}

/// Flex layout properties. Applied via `.width()/.height()/.flex()/.max_width()/.min_height()`.
#[derive(Clone, Debug, Default)]
pub struct FlexStyle {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub flex_grow: f32,
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    pub min_height: Option<f32>,
    pub max_height: Option<f32>,
}

/// Visual decoration (background + border + border-radius + padding).
/// Applied via `.background()/.border()/.rounded()/.padding()`.
#[derive(Clone, Debug, Default)]
pub struct Decoration {
    pub background: Option<Color>,
    pub border_color: Option<Color>,
    pub border_width: f32,
    pub corner_radius: f32,
    pub padding: f32,
}

/// A declarative UI node. Built fresh each frame from app state (pure).
#[derive(Clone, Debug)]
pub enum View {
    Text { text: String, color: Option<Color>, size: f32 },
    Button { label: String, id: Id },
    TextEdit { id: Id, text: String, hint: String, multiline: bool, desired_rows: usize },
    Circle { radius: f32, color: Color },
    Image { src: ImageSource, width: f32, height: f32 },
    Column { children: Vec<View>, spacing: f32 },
    Row { children: Vec<View>, spacing: f32 },
    Container { color: Option<Color>, padding: f32, child: Box<View> },
    Sized { style: FlexStyle, child: Box<View> },
    Decorated { decoration: Decoration, child: Box<View> },
    ScrollView { child: Box<View>, stick_to_bottom: bool },
}

impl Default for View {
    fn default() -> Self {
        View::Column { children: Vec::new(), spacing: 0.0 }
    }
}

// ── PascalCase constructors (Flutter-like) ────────────────────────────────────

/// `Text("hi")` — a text line.
pub fn Text(s: impl Into<String>) -> View {
    View::Text { text: s.into(), color: None, size: 14.0 }
}

/// `Button("Send", ID_SEND)` — a button; `id` is reported on click.
pub fn Button(label: impl Into<String>, id: Id) -> View {
    View::Button { label: label.into(), id }
}

/// `TextEdit(ID_MSG, &self.msg)` — a single-line text field.
pub fn TextEdit(id: Id, text: &str) -> View {
    View::TextEdit { id, text: text.into(), hint: String::new(), multiline: false, desired_rows: 1 }
}

/// `TextMultiline(ID_MSG, &self.msg, 5)` — a multi-line text editor.
pub fn TextMultiline(id: Id, text: &str, desired_rows: usize) -> View {
    View::TextEdit { id, text: text.into(), hint: String::new(), multiline: true, desired_rows }
}

/// `Circle(64.0, Color::CORAL)` — a filled circle (pet body).
pub fn Circle(radius: f32, color: Color) -> View {
    View::Circle { radius, color }
}

/// `Image("/path/pet.png", 200.0, 200.0)` — a raster image from a file path.
pub fn Image(src: impl Into<String>, width: f32, height: f32) -> View {
    View::Image { src: ImageSource::Path(src.into()), width, height }
}

/// `ImageBytes(assets::IDLE_PNG, 200.0, 200.0)` — a raster image from embedded bytes.
pub fn ImageBytes(src: &'static [u8], width: f32, height: f32) -> View {
    View::Image { src: ImageSource::Bytes(src), width, height }
}

/// `ImageSrc(app.skin.clone(), 200.0, 200.0)` — a raster image from an already-resolved [`ImageSource`].
pub fn ImageSrc(src: ImageSource, width: f32, height: f32) -> View {
    View::Image { src, width, height }
}

/// `Column(vec![...])` — vertical stack. See also the [`column!`] macro.
pub fn Column(children: Vec<View>) -> View {
    View::Column { children, spacing: 0.0 }
}

/// `Row(vec![...])` — horizontal stack. See also the [`row!`] macro.
pub fn Row(children: Vec<View>) -> View {
    View::Row { children, spacing: 0.0 }
}

/// `ScrollView(child)` — a vertical scrollable viewport.
pub fn ScrollView(child: View) -> View {
    View::ScrollView { child: Box::new(child), stick_to_bottom: false }
}

impl View {
    #[must_use]
    pub fn color(mut self, c: Color) -> Self {
        match &mut self {
            View::Text { color, .. } => *color = Some(c),
            View::Container { color: bg, .. } => *bg = Some(c),
            _ => {}
        }
        self
    }

    #[must_use]
    pub fn size(mut self, pts: f32) -> Self {
        if let View::Text { size, .. } = &mut self { *size = pts; }
        self
    }

    #[must_use]
    pub fn spacing(mut self, s: f32) -> Self {
        match &mut self {
            View::Column { spacing, .. } | View::Row { spacing, .. } => *spacing = s,
            _ => {}
        }
        self
    }

    #[must_use]
    pub fn stick_to_bottom(mut self) -> Self {
        if let View::ScrollView { stick_to_bottom, .. } = &mut self { *stick_to_bottom = true; }
        self
    }

    // ── Flex builders ──

    fn wrap_flex(self, f: impl FnOnce(&mut FlexStyle)) -> Self {
        match self {
            View::Sized { mut style, child } => { f(&mut style); View::Sized { style, child } }
            other => {
                let mut style = FlexStyle::default();
                f(&mut style);
                View::Sized { style, child: Box::new(other) }
            }
        }
    }

    #[must_use] pub fn flex(self, grow: f32) -> Self { self.wrap_flex(|s| s.flex_grow = grow) }
    #[must_use] pub fn width(self, w: f32) -> Self { self.wrap_flex(|s| s.width = Some(w)) }
    #[must_use] pub fn height(self, h: f32) -> Self { self.wrap_flex(|s| s.height = Some(h)) }
    #[must_use] pub fn max_width(self, w: f32) -> Self { self.wrap_flex(|s| s.max_width = Some(w)) }
    #[must_use] pub fn min_width(self, w: f32) -> Self { self.wrap_flex(|s| s.min_width = Some(w)) }
    #[must_use] pub fn max_height(self, h: f32) -> Self { self.wrap_flex(|s| s.max_height = Some(h)) }
    #[must_use] pub fn min_height(self, h: f32) -> Self { self.wrap_flex(|s| s.min_height = Some(h)) }

    // ── Decoration builders ──

    #[must_use]
    pub fn padding(self, p: f32) -> Self {
        View::Container { color: None, padding: p, child: Box::new(self) }
    }

    fn wrap_decoration(self, f: impl FnOnce(&mut Decoration)) -> Self {
        match self {
            View::Decorated { mut decoration, child } => { f(&mut decoration); View::Decorated { decoration, child } }
            other => {
                let mut decoration = Decoration::default();
                f(&mut decoration);
                View::Decorated { decoration, child: Box::new(other) }
            }
        }
    }

    #[must_use] pub fn background(self, c: Color) -> Self { self.wrap_decoration(|d| d.background = Some(c)) }
    #[must_use] pub fn border(self, color: Color, width: f32) -> Self {
        self.wrap_decoration(|d| { d.border_color = Some(color); d.border_width = width; })
    }
    #[must_use] pub fn rounded(self, radius: f32) -> Self { self.wrap_decoration(|d| d.corner_radius = radius) }
}

/// `Column![a, b, c]` → [`View::Column`] of `[a, b, c]`.
#[macro_export]
macro_rules! column {
    ($($x:expr),* $(,)?) => { $crate::Column(vec![ $($x),* ]) };
}

/// `Row![a, b, c]` → [`View::Row`] of `[a, b, c]`.
#[macro_export]
macro_rules! row {
    ($($x:expr),* $(,)?) => { $crate::Row(vec![ $($x),* ]) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_and_chaining_build_expected_tree() {
        let v: View = column![
            Text("hi").color(Color::WHITE).size(20.0),
            Button("ok", 7),
            TextEdit(2, "x"),
        ];
        match v {
            View::Column { children, .. } => {
                assert_eq!(children.len(), 3);
                assert!(matches!(children[1], View::Button { id: 7, .. }));
            }
            _ => panic!("expected column"),
        }
    }

    #[test]
    fn skin_source_resolves_bundled_default_to_path_in_dev() {
        match skin_source("default/idle.png") {
            ImageSource::Path(p) => assert!(p.ends_with("assets/skins/default/idle.png"), "{p}"),
            ImageSource::Bytes(_) => panic!("expected dev path, got bundled fallback"),
        }
    }

    #[test]
    fn skin_source_missing_file_falls_back_to_bundle() {
        let got = skin_source("__nope__/missing.png");
        let _ = std::fs::remove_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/skins/__nope__"));
        match got {
            ImageSource::Bytes(b) => assert_eq!(b.len(), assets::IDLE_PNG.len()),
            ImageSource::Path(p) => panic!("expected fallback, resolved {p}"),
        }
    }
}
