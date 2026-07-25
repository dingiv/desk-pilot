//! Wires the core FSM + CPU renderer into a runnable `App`, and declares the
//! pet's UI (egui path) as a pure `ui::View` tree.
//!
//! **audio-aura integration:** a background SSE client thread connects to the
//! aura daemon (`GET /api/stream` on `127.0.0.1:9091`), parses live ASR
//! `TurnEvent`s (interim partials + final utterances + intents), and pushes them
//! through an `mpsc` channel. `view()` drains the channel each frame (via
//! `try_recv`, which is `&self`) and declares the transcript panel below the
//! pet body — a status dot, the live interim partial (gray), and recent
//! finalized utterances (white) with their intent tags.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::Duration;

use core::{geometry::Vec2, Canvas, Color, Config, Fsm, Scene};
use platform::{App, InputRegion, PlatformEvent};
use render::{CpuRenderer, Renderer};
use ui::{column, image_src, scroll_view, text, ImageSource, Msg, View};

/// The aura daemon's SSE address (loaded from familiar.yaml at startup).
const AURA_ADDR_DEFAULT: &str = "127.0.0.1:9091";

// ── ASR transcript state ─────────────────────────────────────────────────────

/// A transcript update pushed from the SSE client thread.
enum AsrUpdate {
    Interim(String),
    Final { text: String, intent: String },
    Connected,
    Disconnected,
}

/// The live transcript state, drained from the SSE channel each frame.
struct AsrState {
    connected: bool,
    interim: String,
    /// (text, intent) — newest last.
    history: Vec<(String, String)>,
}

impl Default for AsrState {
    fn default() -> Self {
        Self { connected: false, interim: String::new(), history: Vec::new() }
    }
}

// ── SSE client ────────────────────────────────────────────────────────────────

/// Spawn a background thread that connects to the aura daemon's SSE stream
/// (`GET /api/stream`), parses `TurnEvent` JSON, and pushes `AsrUpdate`s.
/// Reconnects every 2 s on failure. Sends `Connected`/`Disconnected` on
/// transition so the UI can show the link status.
fn spawn_asr_client(addr: String, tx: mpsc::Sender<AsrUpdate>) {
    let _ = std::thread::Builder::new()
        .name("familiar-asr-sse".into())
        .spawn(move || {
            let req = b"GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
            loop {
                match TcpStream::connect(&addr) {
                    Ok(mut stream) => {
                        let _ = stream.write_all(req);
                        let _ = tx.send(AsrUpdate::Connected);
                        for line in BufReader::new(stream).lines() {
                            let line = match line { Ok(l) => l, Err(_) => break };
                            if let Some(json) = line.strip_prefix("data: ") {
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
                                    parse_sse_event(&v, &tx);
                                }
                            }
                        }
                        let _ = tx.send(AsrUpdate::Disconnected);
                    }
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

/// Extract transcript text from a TurnEvent JSON value + push an `AsrUpdate`.
fn parse_sse_event(v: &serde_json::Value, tx: &mpsc::Sender<AsrUpdate>) {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "interim" => {
            if let Some(p) = v.get("partial").and_then(|p| p.as_str()) {
                let _ = tx.send(AsrUpdate::Interim(p.to_string()));
            }
        }
        "final" => {
            // calibrated (Stage2 rewrite) is authoritative; fall back to raw ASR.
            let text = v
                .get("calibrated")
                .filter(|c| c.as_str().map_or(false, |s| !s.trim().is_empty()))
                .or_else(|| v.get("raw_text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let intent = v.get("intent").and_then(|i| i.as_str()).unwrap_or("");
            if !text.is_empty() {
                let _ = tx.send(AsrUpdate::Final { text: text.to_string(), intent: intent.to_string() });
            }
        }
        _ => {}
    }
}

// ── PetApp ───────────────────────────────────────────────────────────────────

pub struct PetApp {
    scene: Scene,
    fsm: Fsm,
    renderer: CpuRenderer,
    /// The pet's body image, resolved once at startup (FileLoader → bundled fallback).
    skin: ImageSource,
    /// ASR transcript channel (drained in `view`).
    asr_rx: mpsc::Receiver<AsrUpdate>,
    /// Transcript state (interior-mutable because `view` is `&self`).
    asr: RefCell<AsrState>,
}

impl PetApp {
    /// Build from `familiar.yaml` fields (loaded by main.rs via FileLoader).
    pub fn with_config(aura_addr: &str, skin: &str) -> Self {
        let cfg = Config::default();
        let mut scene = Scene::demo(cfg.canvas_size);
        scene.bg = cfg.bg;
        scene.pet.color = cfg.pet_color;
        let (tx, rx) = mpsc::channel();
        spawn_asr_client(aura_addr.to_string(), tx);
        Self {
            scene,
            fsm: Fsm::new(),
            renderer: CpuRenderer::new(),
            skin: ui::skin_source(skin),
            asr_rx: rx,
            asr: RefCell::new(AsrState::default()),
        }
    }

    /// Default constructor (used by headless / tests).
    pub fn demo() -> Self {
        Self::with_config(AURA_ADDR_DEFAULT, "default/idle.png")
    }
}

impl App for PetApp {
    fn canvas_size(&self) -> (u32, u32) {
        self.scene.canvas_size
    }

    fn input_region(&self) -> InputRegion {
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

    fn view(&self) -> View {
        // Drain ASR updates (try_recv is &self — safe from &self view).
        while let Ok(upd) = self.asr_rx.try_recv() {
            let mut s = self.asr.borrow_mut();
            match upd {
                AsrUpdate::Interim(t) => s.interim = t,
                AsrUpdate::Final { text, intent } => {
                    s.interim.clear();
                    s.history.push((text, intent));
                    if s.history.len() > 20 {
                        s.history.remove(0);
                    }
                }
                AsrUpdate::Connected => s.connected = true,
                AsrUpdate::Disconnected => {
                    s.connected = false;
                    s.interim.clear();
                }
            }
        }

        let s = self.asr.borrow();
        let mut nodes = vec![image_src(self.skin.clone(), 200.0, 200.0)];

        // Status indicator
        let (dot, label, color) = if s.connected {
            ("●", "ASR live", Color::rgba(0x4f, 0xef, 0x6f, 0xff))
        } else {
            ("○", "ASR off", Color::rgba(0xef, 0x6f, 0x6f, 0xff))
        };
        nodes.push(text(format!("{dot} {label}")).color(color));

        // Live interim partial (gray)
        if !s.interim.is_empty() {
            nodes.push(text(s.interim.clone()).color(Color::rgba(0xaa, 0xaa, 0xaa, 0xff)));
        }

        // Scrollable transcript history (white, newest last, all items) — fills
        // remaining vertical space (flex) + sticks to bottom for newest content.
        let history_nodes: Vec<View> = s.history.iter().map(|(utt, intent)| {
            let label = if intent.is_empty() || intent == "chat" {
                utt.clone()
            } else {
                format!("[{intent}] {utt}")
            };
            text(label).color(Color::WHITE)
        }).collect();
        if !history_nodes.is_empty() {
            nodes.push(
                scroll_view(column(history_nodes).spacing(4.0))
                    .stick_to_bottom()
                    .flex(1.0),
            );
        }

        column(nodes).spacing(6.0)
    }

    fn update(&mut self, _msg: Msg) {}
}

fn hit_pet(scene: &Scene, p: Vec2) -> bool {
    let r = scene.pet.bounding_rect();
    p.x >= r.x && p.x <= r.x + r.w && p.y >= r.y && p.y <= r.y + r.h
}
