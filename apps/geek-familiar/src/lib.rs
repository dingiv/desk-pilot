//! geek-familiar — the desktop pet, rendered by iced.
//!
//! Pure Elm: [`PetApp`] holds state, [`PetApp::view`] declares the UI, [`PetApp::update`]
//! handles messages. The ASR SSE stream is a [`PetApp::subscription`] (wakes iced on each
//! event); the gnome-layer-ext handshake (always-on-top + skip-taskbar/Activities) runs as
//! a startup [`iced::Task`]. The whole rendering/windowing/IME stack is iced's — no GTK4,
//! no egui, cross-platform.
//!
//! UI shape (hover-dock HUD): the pet sprite sits on top (grab it to drag the window).
//! A rounded button dock floats below it. Hovering a dock button expands a rounded panel
//! beneath the dock — `Chat` shows the ASR transcript, `Settings` shows the settings panel.

mod asr;
// Alpha click-through (wl_surface input region) is deferred — module kept on disk for later.
// mod input_region;

use iced::widget::{button, column, container, image, mouse_area, row, scrollable, text, text_input};
use iced::widget::image::Handle;
use iced::{mouse, window, Background, Border, Color, ContentFit, Element, Length, Subscription, Task};

pub use asr::AsrUpdate;

/// Bundled fallback skin (used when FileLoader can't resolve the configured skin).
const IDLE_PNG: &[u8] = include_bytes!("../assets/skins/default/idle.png");

/// Which dock tab is currently open (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Chat,
    Settings,
}

pub struct PetApp {
    aura_addr: String,
    skin: Handle,
    /// `desktop-pet#<token>` window title — gnome-layer-ext matches this prefix.
    token: String,
    asr: AsrState,
    /// The currently open dock tab (`None` = collapsed).
    active_panel: Option<Panel>,
    /// Chat-panel text input (also the IME smoke test).
    ime_input: String,
    /// Settings-panel stub state.
    auto_move: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Asr(AsrUpdate),
    HandshakeDone(bool),
    DragStarted,
    /// Click a dock button: opens that tab, or closes it if already open.
    TabPressed(Panel),
    ImeInput(String),
    ToggleAutoMove,
}

#[derive(Default)]
struct AsrState {
    connected: bool,
    interim: String,
    /// (text, intent) — newest last.
    history: Vec<(String, String)>,
}

impl PetApp {
    /// Boot: build state + run the gnome-layer-ext handshake as a startup Task.
    pub fn new(aura_addr: String, skin: Handle, token: String) -> (Self, Task<Message>) {
        let app = PetApp {
            aura_addr,
            skin,
            token,
            asr: AsrState::default(),
            active_panel: None,
            ime_input: String::new(),
            auto_move: false,
        };
        let token_for_task = app.token.clone();
        let handshake = Task::perform(handshake(token_for_task), Message::HandshakeDone);
        (app, handshake)
    }

    /// Window title — gnome-layer-ext matches the `desktop-pet#` prefix.
    pub fn title(&self) -> String {
        format!("desktop-pet#{}", self.token)
    }

    /// Theme with a TRANSPARENT background — iced clears the window to the theme's
    /// `background` color, so the default (opaque) theme makes the window a solid
    /// rectangle. A transparent background lets the desktop show through everywhere
    /// except where widgets (sprite, dock, panel) actually paint.
    pub fn theme(&self) -> iced::Theme {
        iced::Theme::custom(
            "geek-familiar-transparent",
            iced::theme::Palette {
                background: Color::TRANSPARENT,
                ..iced::Theme::Dark.palette()
            },
        )
    }

    pub fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Asr(u) => match u {
                AsrUpdate::Interim(t) => self.asr.interim = t,
                AsrUpdate::Final { text, intent } => {
                    self.asr.interim.clear();
                    self.asr.history.push((text, intent));
                    if self.asr.history.len() > 20 {
                        self.asr.history.remove(0);
                    }
                }
                AsrUpdate::Connected => self.asr.connected = true,
                AsrUpdate::Disconnected => {
                    self.asr.connected = false;
                    self.asr.interim.clear();
                }
            },
            Message::HandshakeDone(ok) => eprintln!("[geek-familiar] gnome-layer-ext handshake ok={ok}"),
            Message::DragStarted => {
                // Ask the compositor to move the (oldest = main) window.
                return window::oldest().then(|id| match id {
                    Some(id) => window::drag(id),
                    None => Task::none(),
                });
            }
            Message::TabPressed(p) => {
                // Toggle: open the clicked tab, or close it if it's already open.
                self.active_panel = if self.active_panel == Some(p) { None } else { Some(p) };
            }
            Message::ImeInput(s) => self.ime_input = s,
            Message::ToggleAutoMove => self.auto_move = !self.auto_move,
        }
        Task::none()
    }

    /// ASR SSE client as a passive subscription — iced wakes on each event.
    /// `run_with(addr, recipe)` keys the subscription on the daemon address and
    /// rebuilds the stream (boxed, since `run_with` takes a `fn` pointer).
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::run_with(self.aura_addr.clone(), asr_stream)
    }

    pub fn view(&self) -> Element<'_, Message> {
        // ── Pet sprite = drag handle (grab the pet to move the window). ──
        let sprite = mouse_area(
            image(self.skin.clone())
                .width(160)
                .height(160)
                .content_fit(ContentFit::Contain),
        )
        .on_press(Message::DragStarted)
        .interaction(mouse::Interaction::Grab);

        // ── Button dock: rounded row; hover a button to expand its panel. ──
        let dock = container(
            row![
                dock_button("💬 chat", Panel::Chat, self.active_panel == Some(Panel::Chat)),
                dock_button("⚙ 设置", Panel::Settings, self.active_panel == Some(Panel::Settings)),
            ]
            .spacing(4),
        )
        .padding(4)
        .style(|_theme| card_style(0.78));

        let mut col = column![sprite, dock]
            .align_x(iced::alignment::Horizontal::Left)
            .spacing(6);

        // ── Tab panel beneath the dock (click-toggled, stays open until clicked away). ──
        if let Some(panel) = self.active_panel {
            let body: Element<Message> = match panel {
                Panel::Chat => self.chat_panel(),
                Panel::Settings => self.settings_panel(),
            };
            col = col.push(container(body).style(|_theme| card_style(0.92)));
        }

        col.into()
    }

    /// Chat panel: ASR status + live interim + scrollable transcript + text input.
    fn chat_panel(&self) -> Element<Message> {
        let (dot, label, color) = if self.asr.connected {
            ("●", "ASR live", Color::from_rgb8(0x4f, 0xef, 0x6f))
        } else {
            ("○", "ASR off", Color::from_rgb8(0xef, 0x6f, 0x6f))
        };

        let mut rows: Vec<Element<Message>> = vec![text(format!("{dot} {label}")).color(color).into()];

        if !self.asr.interim.is_empty() {
            rows.push(text(self.asr.interim.clone()).color(Color::from_rgb8(0xaa, 0xaa, 0xaa)).into());
        }

        if !self.asr.history.is_empty() {
            let items: Vec<Element<Message>> = self
                .asr
                .history
                .iter()
                .map(|(utt, intent)| {
                    let label = if intent.is_empty() || intent == "chat" {
                        utt.clone()
                    } else {
                        format!("[{intent}] {utt}")
                    };
                    text(label).color(Color::WHITE).into()
                })
                .collect();
            rows.push(
                scrollable(column(items).spacing(2))
                    .anchor_bottom()
                    .height(Length::Fixed(120.0))
                    .into(),
            );
        }

        rows.push(
            text_input("type a message…", &self.ime_input)
                .on_input(Message::ImeInput)
                .into(),
        );

        column(rows).spacing(4).padding(6).into()
    }

    /// Settings panel (stub): a couple of toggles. Real settings wired later.
    fn settings_panel(&self) -> Element<Message> {
        column![
            text("设置").color(Color::WHITE).size(14),
            button(text(if self.auto_move { "☑ 自动游走" } else { "☐ 自动游走" }).color(Color::WHITE))
                .on_press(Message::ToggleAutoMove)
                .padding([2, 6]),
            text(format!("aura: {}", self.aura_addr)).color(Color::from_rgb8(0x88, 0x88, 0x88)).size(11),
        ]
        .spacing(4)
        .padding(6)
        .into()
    }
}

/// A rounded dock tab button. `active` highlights the open tab. Click toggles it.
fn dock_button(label: &'static str, panel: Panel, active: bool) -> Element<'static, Message> {
    mouse_area(
        container(text(label).size(13).color(Color::WHITE))
            .width(70.0)
            .align_x(iced::alignment::Horizontal::Center)
            .padding([4, 0])
            .style(move |_theme| pill_style(active)),
    )
    .on_press(Message::TabPressed(panel))
    .interaction(mouse::Interaction::Pointer)
    .into()
}

/// Pill style for a dock tab. Accent when active, dark otherwise.
fn pill_style(active: bool) -> container::Style {
    let bg = if active {
        Color::from_rgba(0.35, 0.55, 0.95, 0.95)
    } else {
        Color::from_rgba(0.20, 0.20, 0.26, 0.85)
    };
    container::Style {
        background: Some(Background::Color(bg)),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 6.0.into() },
        ..Default::default()
    }
}

/// A semi-transparent dark rounded-card container style. `alpha` ∈ [0, 1].
fn card_style(alpha: f32) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgba(0.12, 0.12, 0.18, alpha))),
        border: Border { color: Color::from_rgba(0.4, 0.4, 0.5, alpha * 0.8), width: 1.0, radius: 8.0.into() },
        ..Default::default()
    }
}

/// Build the ASR SSE stream for a given daemon address. Returns a boxed stream
/// because [`Subscription::run_with`] takes a `fn` pointer (can't return `impl Trait`).
fn asr_stream(addr: &String) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    let addr = addr.clone();
    Box::pin(iced::stream::channel::<Message>(100, move |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
        asr::spawn(addr, move |upd| {
            let _ = sender.try_send(Message::Asr(upd));
        });
    }))
}

/// Resolve the skin image via FileLoader (`SKIN::<rel>`), fall back to bundled bytes.
pub fn skin_source(rel: &str) -> Handle {
    let loader = fs::loader!();
    match loader.resolve(&format!("SKIN::{rel}")).filter(|p| p.exists()) {
        Some(p) => {
            eprintln!("[geek-familiar] skin: {}", p.display());
            Handle::from_path(p.to_string_lossy().into_owned())
        }
        None => {
            eprintln!("[geek-familiar] skin: {rel} not found, bundled fallback");
            Handle::from_bytes(IDLE_PNG)
        }
    }
}

/// One-shot gnome-layer-ext handshake over a Unix socket (async, runs once at boot).
async fn handshake(token: String) -> bool {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let sock = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/gnome-layer-ext.sock"))
        .unwrap_or_else(|_| "/run/user/1000/gnome-layer-ext.sock".into());
    match UnixStream::connect(&sock) {
        Ok(mut s) => {
            let req = format!("{{\"v\":1,\"token\":\"{token}\",\"app_id\":\"geek-familiar\"}}\n");
            if s.write_all(req.as_bytes()).is_err() {
                return false;
            }
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            resp.contains("\"ok\":true")
        }
        Err(_) => false,
    }
}
