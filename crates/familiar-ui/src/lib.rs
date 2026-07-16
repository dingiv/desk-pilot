//! Declarative UI layer — a renderer-agnostic [`View`] tree + [`Msg`] events.
//!
//! Business logic declares *what* the UI looks like (a [`View`] tree, built
//! fresh each frame from state, Flutter/Elm-style); a renderer (the egui binder
//! in `platform`) walks the tree to paint it and reports interactions back as
//! [`Msg`]s. This keeps UI declaration (frequently-changing business logic, and
//! later user-customizable appearance) separate from the render layer — swapping
//! renderers, or data-driving the `View` tree for theming, needs no change to
//! business logic or to the renderer.
//!
//! `View` is pure data (no `&mut`, no renderer types); `Msg` is the only
//! feedback channel. `Id` lets the app map a widget back to a semantic action in
//! its `update`.

pub use core::Color;

/// Bundled pet assets (embedded at compile time via `include_bytes!`).
pub mod assets {
    /// The default idle form — a high-res irregular-shape PNG with transparent surround.
    pub const IDLE_PNG: &[u8] = include_bytes!("../assets/skins/default/idle.png");
}

/// Resolve a skin file (`"<skin>/<file>"`, e.g. `"default/idle.png"`) to an
/// [`ImageSource`] via the `SKIN` namespace (declared in this crate's
/// `Cargo.toml`): dev → `assets/skins/`, prod → `~/.geek-familiar/skins/`.
/// Falls back to the bundled [`assets::IDLE_PNG`] when the file is missing, so
/// the pet always renders. Resolve once at startup, not per frame.
pub fn skin_source(rel: &str) -> ImageSource {
    let loader = fs::loader!();
    // resolve() on a namespace returns the path unchecked (and creates parent
    // dirs) — gate on existence so a missing skin falls back to the bundle.
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
    /// A filesystem path (loaded at runtime).
    Path(String),
    /// Embedded bytes (e.g. from [`assets::IDLE_PNG`] / `include_bytes!`).
    Bytes(&'static [u8]),
}

impl ImageSource {
    /// A stable cache key for this source.
    pub fn cache_key(&self) -> String {
        match self {
            ImageSource::Path(p) => p.clone(),
            ImageSource::Bytes(b) => format!("bytes:{:p}", b.as_ptr()),
        }
    }
}

/// Stable id for an interactive widget (button / text field). The app maps ids
/// to semantics in its `update`.
pub type Id = u64;

/// An interaction, reported by the renderer back to business logic.
#[derive(Clone, Debug)]
pub enum Msg {
    /// A button identified by `id` was clicked.
    Clicked(Id),
    /// A text field (`id`) changed to `text`.
    TextChanged(Id, String),
}

/// A declarative UI node. Built fresh each frame from app state (pure) and
/// handed to a renderer.
#[derive(Clone, Debug)]
pub enum View {
    /// A line of text.
    Text {
        text: String,
        color: Option<Color>,
        size: f32,
    },
    /// A clickable button. `id` is reported via [`Msg::Clicked`] on click.
    Button {
        label: String,
        id: Id,
    },
    /// A single-line text edit. `text` is the current value; edits arrive as
    /// [`Msg::TextChanged`].
    TextEdit {
        id: Id,
        text: String,
        hint: String,
    },
    /// A filled circle — a simple "pet body" shape and a customization hook.
    Circle {
        radius: f32,
        color: Color,
    },
    /// A raster image (high-res PNG with a transparent surround is the intended
    /// asset form). `src` identifies the asset; `width`/`height` are the display
    /// size in px. The renderer pre-resizes with a high-quality filter so
    /// downscaling stays crisp. Different pet forms = different `src`s (the app
    /// switches between them); alignment between forms is the asset author's job.
    Image {
        src: ImageSource,
        width: f32,
        height: f32,
    },
    /// Stack children vertically.
    Column {
        children: Vec<View>,
    },
    /// Stack children horizontally.
    Row {
        children: Vec<View>,
    },
    /// Pad + optionally color behind `child`.
    Container {
        color: Option<Color>,
        padding: f32,
        child: Box<View>,
    },
    /// Force a fixed size around `child` (None = unbounded on that axis).
    SizedBox {
        width: Option<f32>,
        height: Option<f32>,
        child: Box<View>,
    },
}

impl Default for View {
    /// An empty UI (no nodes).
    fn default() -> Self {
        View::Column { children: Vec::new() }
    }
}

// ── constructors (Flutter-like ergonomics) ───────────────────────────────────

/// `text("hi")` — a text line.
pub fn text(s: impl Into<String>) -> View {
    View::Text { text: s.into(), color: None, size: 14.0 }
}

/// `button("Send", ID_SEND)` — a button; `id` is reported on click.
pub fn button(label: impl Into<String>, id: Id) -> View {
    View::Button { label: label.into(), id }
}

/// `text_edit(ID_MSG, &self.msg)` — a single-line text field.
pub fn text_edit(id: Id, text: &str) -> View {
    View::TextEdit { id, text: text.into(), hint: String::new() }
}

/// `circle(64.0, Color::CORAL)` — a filled circle (pet body).
pub fn circle(radius: f32, color: Color) -> View {
    View::Circle { radius, color }
}

/// `image("/path/pet.png", 200.0, 200.0)` — a raster image from a file path.
pub fn image(src: impl Into<String>, width: f32, height: f32) -> View {
    View::Image { src: ImageSource::Path(src.into()), width, height }
}

/// `image_bytes(assets::IDLE_PNG, 200.0, 200.0)` — a raster image from embedded
/// bytes (e.g. bundled at compile time via `include_bytes!`).
pub fn image_bytes(src: &'static [u8], width: f32, height: f32) -> View {
    View::Image { src: ImageSource::Bytes(src), width, height }
}

/// `image_src(app.skin.clone(), 200.0, 200.0)` — a raster image from an
/// already-resolved [`ImageSource`] (e.g. from [`skin_source`]).
pub fn image_src(src: ImageSource, width: f32, height: f32) -> View {
    View::Image { src, width, height }
}

/// `column(vec![...])` — vertical stack. See also the [`column!`] macro.
pub fn column(children: Vec<View>) -> View {
    View::Column { children }
}

/// `row(vec![...])` — horizontal stack. See also the [`row!`] macro.
pub fn row(children: Vec<View>) -> View {
    View::Row { children }
}

impl View {
    /// Set the text color (Text) or background (Container). No-op elsewhere.
    #[must_use]
    pub fn color(mut self, c: Color) -> Self {
        match &mut self {
            View::Text { color, .. } => *color = Some(c),
            View::Container { color: bg, .. } => *bg = Some(c),
            _ => {}
        }
        self
    }

    /// Set the text font size (Text), in points.
    #[must_use]
    pub fn size(mut self, pts: f32) -> Self {
        if let View::Text { size, .. } = &mut self {
            *size = pts;
        }
        self
    }

    /// Wrap in a [`View::Container`] with the given padding.
    #[must_use]
    pub fn padding(self, p: f32) -> Self {
        View::Container { color: None, padding: p, child: Box::new(self) }
    }

    /// Wrap in a [`View::SizedBox`] with a fixed width.
    #[must_use]
    pub fn width(self, w: f32) -> Self {
        View::SizedBox { width: Some(w), height: None, child: Box::new(self) }
    }
}

/// `column![a, b, c]` → [`View::Column`] of `[a, b, c]` (like `vec!`).
#[macro_export]
macro_rules! column {
    ($($x:expr),* $(,)?) => { $crate::column(vec![ $($x),* ]) };
}

/// `row![a, b, c]` → [`View::Row`] of `[a, b, c]`.
#[macro_export]
macro_rules! row {
    ($($x:expr),* $(,)?) => { $crate::row(vec![ $($x),* ]) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_and_chaining_build_expected_tree() {
        let v: View = column![
            text("hi").color(Color::WHITE).size(20.0),
            button("ok", 7),
            text_edit(2, "x"),
        ];
        match v {
            View::Column { children } => {
                assert_eq!(children.len(), 3);
                assert!(matches!(children[1], View::Button { id: 7, .. }));
            }
            _ => panic!("expected column"),
        }
    }

    #[test]
    fn skin_source_resolves_bundled_default_to_path_in_dev() {
        // Under cargo test the SKIN dev root (assets/skins/) holds default/idle.png.
        match skin_source("default/idle.png") {
            ImageSource::Path(p) => assert!(p.ends_with("assets/skins/default/idle.png"), "{p}"),
            ImageSource::Bytes(_) => panic!("expected dev path, got bundled fallback"),
        }
    }

    #[test]
    fn skin_source_missing_file_falls_back_to_bundle() {
        let got = skin_source("__nope__/missing.png");
        // resolve() creates the namespace parent dir as a side effect — clean it up.
        let _ = std::fs::remove_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/skins/__nope__"));
        match got {
            // IDLE_PNG is a `const` (inlined per use), so compare contents not pointers.
            ImageSource::Bytes(b) => assert_eq!(b.len(), assets::IDLE_PNG.len()),
            ImageSource::Path(p) => panic!("expected fallback, resolved {p}"),
        }
    }
}
