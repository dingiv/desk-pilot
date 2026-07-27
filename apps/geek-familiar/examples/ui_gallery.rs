//! UI Gallery — a standalone window for testing UI components (widget gallery /
//! storybook pattern). Completely decoupled from the pet's business logic: no
//! ASR, no skin, no config. Just exercises every View variant + the flex layout.
//!
//! Run: cargo run -p geek-familiar --example ui_gallery --features egui

use std::time::Duration;

use core::{Canvas, Color};
use platform::{App, InputRegion, PlatformBackend, PlatformEvent};
use ui::*;

fn main() {
    #[cfg(feature = "gtk")]
    {
        let mut backend = platform::gtk::GtkBackend::new();
        backend.run(Box::new(GalleryApp::new()));
    }
    #[cfg(not(feature = "gtk"))]
    {
        let mut backend = platform::HeadlessBackend::default();
        backend.run(Box::new(GalleryApp::new()));
    }
}

/// Gallery app state — interactive widgets have persistent state here.
struct GalleryApp {
    click_count: u32,
    text_single: String,
    text_multi: String,
    scroll_items: Vec<String>,
    /// Toggle which section is visible (for conditional rendering test).
    show_section: bool,
}

impl GalleryApp {
    fn new() -> Self {
        let scroll_items: Vec<String> = (1..=30)
            .map(|i| format!("Line {i}: the quick brown fox jumps over the lazy dog"))
            .collect();
        Self {
            click_count: 0,
            text_single: String::from("hello"),
            text_multi: String::from("Multi-line\neditor\nwith several rows"),
            scroll_items,
            show_section: true,
        }
    }
}

// Widget IDs
const ID_BTN: Id = 1;
const ID_TOGGLE: Id = 2;
const ID_TEXT_SINGLE: Id = 3;
const ID_TEXT_MULTI: Id = 4;

impl App for GalleryApp {
    fn canvas_size(&self) -> (u32, u32) {
        (400, 600)
    }

    fn input_region(&self) -> InputRegion {
        InputRegion::default()
    }

    fn handle_event(&mut self, _ev: &PlatformEvent) {}

    fn tick(&mut self, _dt: Duration) {}

    fn render(&self, _out: &mut Canvas) {}

    fn view(&self) -> View {
        // Opaque background so the alpha-click-through scan covers the whole
        // window (gallery needs ALL pixels interactive, unlike the pet).
        Column(vec![
            // ── Title ──
            Text("UI Gallery").color(Color::WHITE).size(22.0),
            Text("Testing all View variants + flex layout").color(Color::rgba(0xaa, 0xaa, 0xaa, 0xff)),

            // ── Button section ──
            Text("--- Buttons ---").color(Color::rgba(0x88, 0x88, 0xff, 0xff)),
            Row(vec![
                Button(format!("Clicked: {}", self.click_count), ID_BTN),
                Button("Toggle", ID_TOGGLE),
            ]).spacing(8.0),

            // ── Text section ──
            Text("--- Text ---").color(Color::rgba(0x88, 0x88, 0xff, 0xff)),
            Text("Plain text line").color(Color::WHITE),
            Text("Gray secondary").color(Color::rgba(0xaa, 0xaa, 0xaa, 0xff)),
            Text("Large").color(Color::rgba(0xff, 0xcc, 0x00, 0xff)).size(20.0),

            // ── TextEdit section ──
            Text("--- TextBox ---").color(Color::rgba(0x88, 0x88, 0xff, 0xff)),
            TextEdit(ID_TEXT_SINGLE, &self.text_single),
            TextMultiline(ID_TEXT_MULTI, &self.text_multi, 4),

            // ── Flex layout section ──
            Text("--- Flex Layout ---").color(Color::rgba(0x88, 0x88, 0xff, 0xff)),
            Row(vec![
                Text("fixed 80").color(Color::WHITE).width(80.0),
                Text("flex 1").color(Color::rgba(0x4f, 0xef, 0x6f, 0xff)).flex(1.0),
                Text("flex 2").color(Color::rgba(0xef, 0x6f, 0x6f, 0xff)).flex(2.0),
            ]).spacing(4.0),

            // ── Decoration section (background / border / rounded) ──
            Text("--- Decoration ---").color(Color::rgba(0x88, 0x88, 0xff, 0xff)),
            Text("bg only")
                .color(Color::WHITE)
                .background(Color::rgba(0x33, 0x33, 0x44, 0xff)),
            Text("bg + border")
                .color(Color::WHITE)
                .background(Color::rgba(0x33, 0x44, 0x33, 0xff))
                .border(Color::rgba(0x4f, 0xef, 0x6f, 0xff), 1.0),
            Text("bg + border + rounded 8")
                .color(Color::WHITE)
                .background(Color::rgba(0x44, 0x33, 0x33, 0xff))
                .border(Color::rgba(0xef, 0x6f, 0x6f, 0xff), 2.0)
                .rounded(8.0),
            Text("rounded 12 + border + padding")
                .color(Color::WHITE)
                .background(Color::rgba(0x33, 0x33, 0x55, 0xff))
                .border(Color::rgba(0x88, 0x88, 0xff, 0xff), 1.0)
                .rounded(12.0)
                .padding(8.0),
            Row(vec![
                Text("card A")
                    .color(Color::WHITE)
                    .background(Color::rgba(0x22, 0x22, 0x33, 0xff))
                    .border(Color::rgba(0x66, 0x66, 0x88, 0xff), 1.0)
                    .rounded(6.0)
                    .padding(6.0)
                    .flex(1.0),
                Text("card B")
                    .color(Color::WHITE)
                    .background(Color::rgba(0x33, 0x22, 0x22, 0xff))
                    .border(Color::rgba(0x88, 0x66, 0x66, 0xff), 1.0)
                    .rounded(6.0)
                    .padding(6.0)
                    .flex(1.0),
            ]).spacing(6.0),

            // ── Conditional rendering ──
            Text("--- Conditional ---").color(Color::rgba(0x88, 0x88, 0xff, 0xff)),
            if self.show_section {
                Text("▶ Section is ON (click Toggle to hide)").color(Color::rgba(0x4f, 0xef, 0x6f, 0xff))
            } else {
                Text("▶ Section is OFF (click Toggle to show)").color(Color::rgba(0xef, 0x6f, 0x6f, 0xff))
            },

            // ── ScrollView section (the main test target) ──
            Text("--- ScrollView (flex, stick-to-bottom) ---").color(Color::rgba(0x88, 0x88, 0xff, 0xff)),
            ScrollView(
                Column(
                    self.scroll_items.iter().map(|s| Text(s.clone()).color(Color::WHITE)).collect()
                ).spacing(2.0)
            )
            .stick_to_bottom()
            .flex(1.0)
            .min_height(100.0),
        ])
        .spacing(8.0)
        .background(Color::rgba(0x1a, 0x1a, 0x2a, 0xff)) // opaque dark bg
    }

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Clicked(ID_BTN) => self.click_count += 1,
            Msg::Clicked(ID_TOGGLE) => self.show_section = !self.show_section,
            Msg::TextChanged(ID_TEXT_SINGLE, s) => self.text_single = s,
            Msg::TextChanged(ID_TEXT_MULTI, s) => self.text_multi = s,
            _ => {}
        }
    }
}
