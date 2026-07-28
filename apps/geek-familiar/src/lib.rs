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
mod gnome_ext;
mod input_region;

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
    /// Captured window frame (for computing the click-through input region).
    ScreenshotReady(iced::window::screenshot::Screenshot),
    PassthroughApplied(usize),
    /// Periodic tick — re-screenshot + re-apply the input region so it tracks the
    /// rendered content (catches late image decode, panel toggles, ASR growth).
    RescanTick,
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
        let handshake = Task::perform(
            gnome_ext::handshake(token_for_task, "geek-familiar"),
            Message::HandshakeDone,
        );
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
            Message::HandshakeDone(ok) => {
                eprintln!("[geek-familiar] gnome-layer-ext handshake ok={ok}");
                // The periodic RescanTick subscription handles the first capture
                // (after the skin image has decoded + uploaded).
            }
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
                // Layout changed (panel opened/closed) → re-compute the input region.
                return screenshot_oldest();
            }
            Message::ImeInput(s) => self.ime_input = s,
            Message::ToggleAutoMove => self.auto_move = !self.auto_move,
            Message::ScreenshotReady(s) => {
                // Step 2: full alpha scan — every transparent PIXEL passes through
                // (sprite corners, panel rounded corners, gaps, margins); only opaque
                // pixels (sprite body, text, panel body) catch.
                let scale = s.scale_factor;
                let mut rects =
                    input_region::alpha_rects(&s.rgba, s.size.width, s.size.height, 30);
                // Screenshot is physical pixels; wl_surface input region is surface-local (logical).
                if scale != 1.0 {
                    for r in &mut rects {
                        r.x = (r.x as f32 / scale).round() as i32;
                        r.y = (r.y as f32 / scale).round() as i32;
                        r.w = (r.w as f32 / scale).round() as i32;
                        r.h = (r.h as f32 / scale).round() as i32;
                    }
                }
                eprintln!(
                    "[passthrough] {} rects from {}x{} scale {}",
                    rects.len(), s.size.width, s.size.height, scale
                );
                return apply_input_region(rects);
            }
            Message::PassthroughApplied(n) => eprintln!("[passthrough] applied {n} rect(s)"),
            Message::RescanTick => return screenshot_oldest(),
        }
        Task::none()
    }

    /// ASR SSE stream + a periodic re-scan tick (keeps the click-through input
    /// region tracking the rendered content).
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run_with(self.aura_addr.clone(), asr_stream),
            Subscription::run_with("rescan".to_string(), rescan_stream),
        ])
    }

    pub fn view(&self) -> Element<'_, Message> {
        // ── Pet sprite (display-only; drag via the dedicated dock button). ──
        let sprite = image(self.skin.clone())
            .width(160)
            .height(160)
            .content_fit(ContentFit::Contain);

        // ── Button dock: drag handle + tab buttons. ──
        let dock = container(
            row![
                drag_button(),
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
    fn chat_panel(&self) -> Element<'_, Message> {
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

        column(rows).spacing(4).padding(6).width(Length::Fixed(220.0)).into()
    }

    /// Settings panel (stub): a couple of toggles. Real settings wired later.
    fn settings_panel(&self) -> Element<'_, Message> {
        column![
            text("设置").color(Color::WHITE).size(14),
            button(text(if self.auto_move { "☑ 自动游走" } else { "☐ 自动游走" }).color(Color::WHITE))
                .on_press(Message::ToggleAutoMove)
                .padding([2, 6]),
            text(format!("aura: {}", self.aura_addr)).color(Color::from_rgb8(0x88, 0x88, 0x88)).size(11),
        ]
        .spacing(4)
        .padding(6)
        .width(Length::Fixed(220.0))
        .into()
    }
}

/// The dedicated drag handle in the dock. Hover → grab cursor; press → compositor
/// move. (The pet sprite is display-only now.)
fn drag_button() -> Element<'static, Message> {
    mouse_area(
        container(text("⠿").size(15).color(Color::from_rgb8(0xcc, 0xcc, 0xcc)))
            .width(36.0)
            .align_x(iced::alignment::Horizontal::Center)
            .padding([4, 0])
            .style(move |_theme| pill_style(false)),
    )
    .on_press(Message::DragStarted)
    .interaction(mouse::Interaction::Grab)
    .into()
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

/// A ~2 s periodic tick stream so the click-through input region re-scans the
/// rendered frame (catches late skin decode, panel toggles, ASR-history growth).
fn rescan_stream(_id: &String) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    Box::pin(iced::stream::channel::<Message>(4, |sender: iced::futures::channel::mpsc::Sender<Message>| async move {
        std::thread::spawn(move || {
            let mut sender = sender;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(2000));
                let _ = sender.try_send(Message::RescanTick);
            }
        });
    }))
}

// ── click-through helpers ────────────────────────────────────────────────────

/// Capture the main window's frame so we can compute its opaque region.
fn screenshot_oldest() -> Task<Message> {
    window::oldest().then(|id| match id {
        Some(id) => window::screenshot(id).map(Message::ScreenshotReady),
        None => Task::none(),
    })
}

/// Set `rects` as the wl_surface input region of the main window.
fn apply_input_region(rects: Vec<input_region::Rect>) -> Task<Message> {
    window::oldest().then(move |id| match id {
        Some(id) => {
            // `.then` is FnMut → clone the Vec for the inner FnOnce window::run closure.
            let rects = rects.clone();
            window::run(id, move |w| {
                apply_to_window(w, &rects);
                Message::PassthroughApplied(rects.len())
            })
        }
        None => Task::none(),
    })
}

/// Extract the raw Wayland surface/display pointers from an iced window and apply
/// the input region. No-op on non-Wayland.
fn apply_to_window(w: &dyn iced::Window, rects: &[input_region::Rect]) {
    let (Some(wh), Some(dh)) = (w.window_handle().ok(), w.display_handle().ok()) else {
        eprintln!("[passthrough] no window/display handle");
        return;
    };
    let (raw_window_handle::RawWindowHandle::Wayland(sh), raw_window_handle::RawDisplayHandle::Wayland(dh)) =
        (wh.as_raw(), dh.as_raw())
    else {
        eprintln!("[passthrough] not a Wayland window");
        return;
    };
    let (surf, disp) = (sh.surface.as_ptr(), dh.display.as_ptr());
    eprintln!("[passthrough] FFI apply {} rect(s) surf={:p} disp={:p}", rects.len(), surf, disp);
    // SAFETY: surf/disp come from a live iced Wayland window; apply does not
    // destroy the foreign surface (mem::forget inside).
    unsafe {
        input_region::apply(
            surf as *mut std::ffi::c_void,
            disp as *mut std::ffi::c_void,
            rects,
        );
    }
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
