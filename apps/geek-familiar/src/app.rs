//! geek-familiar — the desktop pet, rendered by iced.
//!
//! Architecture: **Model → Service → View**
//! - [`model`] — data types (config, state, messages)
//! - [`service`] — business logic (ASR SSE, aura HTTP, gnome extension)
//! - [`view`] — widget constructors and styling
//!
//! [`PetApp`] is the composition root: it owns the state, wires services
//! into subscriptions and tasks, and delegates rendering to the view layer.

use iced::widget::{column, container, image, mouse_area, row, stack, text};
use iced::alignment::Vertical;
use iced::widget::image::Handle;
use iced::widget::svg::Handle as SvgHandle;
use iced::{Color, ContentFit, Element, Length, Subscription, Task, alignment, window};
use crate::view::card_style;

pub(crate) use crate::model::{AsrState, ConversationTurn, DockingPreference, Message, Panel, StyleConfig};
pub(crate) use crate::service::asr::AsrUpdate;

/// Bundled fallback skin (used when FileLoader can't resolve the configured skin).
const IDLE_PNG: &[u8] = include_bytes!("../assets/skins/default/idle.png");
/// Resize-grip icon (four-pointed star, embedded SVG).
const RESIZOR_SVG: &[u8] = include_bytes!("../assets/skins/default/resizor.dio.svg");

// ── PetApp (composition root) ─────────────────────────────────────────────────

pub struct PetApp {
    pub(crate) aura_addr: String,
    skin: Handle,
    token: String,
    pub(crate) asr: AsrState,
    active_panel: Option<Panel>,
    pub(crate) ime_input: String,
    pub(crate) auto_move: bool,
    pub(crate) font_size: f32,
    pub(crate) sprite_size: f32,
    sprite_filter: String,
    resize_handle: SvgHandle,
    pub(crate) style: StyleConfig,
    window_bg: Option<String>,
    docking: DockingPreference,
}

impl PetApp {
    pub fn new(
        aura_addr: String, skin: Handle, token: String, font_size: f32,
        style: StyleConfig, window_bg: Option<String>, docking: DockingPreference,
        sprite_size: f32, sprite_filter: String,
    ) -> (Self, Task<Message>) {
        let app = PetApp {
            aura_addr, skin, token, font_size, sprite_size, sprite_filter, style, window_bg, docking,
            asr: AsrState::default(),
            active_panel: None, ime_input: String::new(), auto_move: false,
            resize_handle: SvgHandle::from_memory(RESIZOR_SVG),
        };
        let token_for_task = app.token.clone();
        let handshake = Task::perform(
            crate::service::gnome_ext::handshake(token_for_task, "geek-familiar"),
            Message::HandshakeDone,
        );
        (app, handshake)
    }

    pub fn title(&self) -> String { format!("desktop-pet#{}", self.token) }

    pub fn theme(&self) -> iced::Theme {
        let bg = self.window_bg.as_deref().and_then(crate::view::parse_bg).unwrap_or(Color::TRANSPARENT);
        iced::Theme::custom("geek-familiar", iced::theme::Palette {
            background: bg,
            ..iced::Theme::Dark.palette()
        })
    }

    pub fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Asr(u) => match u {
                AsrUpdate::Hello => eprintln!("[geek-familiar] aura SSE hello"),
                AsrUpdate::Interim(t) => self.asr.interim = t,
                AsrUpdate::Final { text, intent, reply, seq } => {
                    self.asr.interim.clear();
                    self.asr.history.push(ConversationTurn { seq, user_text: text, intent, reply });
                    if self.asr.history.len() > 20 { self.asr.history.remove(0); }
                }
                AsrUpdate::Status(connected) => self.asr.connected = connected,
                AsrUpdate::Connected => {}
                AsrUpdate::Disconnected => {
                    self.asr.connected = false;
                    self.asr.interim.clear();
                }
            },
            Message::HandshakeDone(ok) => {
                eprintln!("[geek-familiar] gnome-layer-ext handshake ok={ok}");
                let addr = self.aura_addr.clone();
                return Task::perform(async move { crate::service::aura::health(&addr) }, Message::HealthCheck);
            }
            Message::DragStarted => {
                return window::oldest().then(|id| match id {
                    Some(id) => window::drag(id),
                    None => Task::none(),
                });
            }
            Message::TabPressed(p) => {
                self.active_panel = if self.active_panel == Some(p) { None } else { Some(p) };
                return screenshot_oldest();
            }
            Message::ImeInput(s) => self.ime_input = s,
            Message::ToggleAutoMove => self.auto_move = !self.auto_move,
            Message::ScreenshotReady(s) => {
                let scale = s.scale_factor;
                let mut rects = crate::input_region::alpha_rects(&s.rgba, s.size.width, s.size.height, 30);
                if scale != 1.0 {
                    for r in &mut rects {
                        r.x = (r.x as f32 / scale).round() as i32;
                        r.y = (r.y as f32 / scale).round() as i32;
                        r.w = (r.w as f32 / scale).round() as i32;
                        r.h = (r.h as f32 / scale).round() as i32;
                    }
                }
                eprintln!("[passthrough] {} rects from {}x{} scale {}", rects.len(), s.size.width, s.size.height, scale);
                return apply_input_region(rects);
            }
            Message::PassthroughApplied(n) => eprintln!("[passthrough] applied {n} rect(s)"),
            Message::RescanTick => return screenshot_oldest(),
            Message::ToggleRecording => {
                let enable = !self.asr.connected;
                let addr = self.aura_addr.clone();
                eprintln!("[geek-familiar] recording {}", if enable { "ON" } else { "OFF" });
                return Task::perform(
                    async move { let _ = crate::service::aura::control_scout(&addr, Some(enable)); },
                    |_| Message::RecordingToggled,
                );
            }
            Message::RecordingToggled => {}
            Message::Quit => {
                return window::oldest().then(|id| match id {
                    Some(id) => window::close::<Message>(id),
                    None => Task::none(),
                });
            }
            Message::HealthCheck(reachable) => eprintln!(
                "[geek-familiar] aura daemon {}", if reachable { "reachable" } else { "NOT REACHABLE" }
            ),
            Message::DiagonalResizeStart => {
                let dir = match self.docking {
                    DockingPreference::Left => iced::window::Direction::SouthEast,
                    DockingPreference::Right => iced::window::Direction::SouthWest,
                };
                return window::oldest().then(move |id| match id {
                    Some(id) => window::drag_resize(id, dir),
                    None => Task::none(),
                });
            }
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run_with(self.aura_addr.clone(), asr_stream),
            Subscription::run_with("rescan".to_string(), rescan_stream),
        ])
    }

    pub fn view(&self) -> Element<'_, Message> {
        let style = self.style.clone();
        let fs = self.font_size;

        let sprite = image(self.skin.clone())
            .width(self.sprite_size).height(self.sprite_size)
            .content_fit(ContentFit::Contain)
            .filter_method(if self.sprite_filter == "nearest" {
                iced::widget::image::FilterMethod::Nearest
            } else {
                iced::widget::image::FilterMethod::Linear
            });

        let dock = row![
            crate::view::recording_button(self.asr.connected, fs, style.clone()),
            crate::view::dock_button("💬", Panel::Chat, self.active_panel == Some(Panel::Chat), fs, style.clone()),
            crate::view::dock_button("⚙", Panel::Settings, self.active_panel == Some(Panel::Settings), fs, style.clone()),
            crate::view::drag_button(fs, style.clone()),
        ]
        .spacing(4);

        let (_, grip_align) = match self.docking {
            DockingPreference::Left => (iced::alignment::Horizontal::Left, iced::alignment::Horizontal::Right),
            DockingPreference::Right => (iced::alignment::Horizontal::Right, iced::alignment::Horizontal::Left),
        };

        let mut col = column![sprite, dock].align_x(alignment::Horizontal::Center).spacing(6);

        if let Some(panel) = self.active_panel {
            let body: Element<Message> = match panel {
                Panel::Chat => crate::view::chat_panel(self),
                Panel::Settings => crate::view::settings_panel(self),
            };
            col = col.push(
                container(body).width(Length::Fill)
                    .style({ let s = style.clone(); move |_theme| crate::view::card_style(&s, 0.92) }),
            );
        }

        // Resize grip — fixed to the window corner via Stack overlay so it's
        // always reachable even when the window is too short for the content.
        let corner_grip = mouse_area(
            container(iced::widget::svg::Svg::new(self.resize_handle.clone()).width(12).height(20)).padding([2, 4]),
        )
        .on_press(Message::DiagonalResizeStart)
        .interaction(iced::mouse::Interaction::ResizingDiagonallyUp);

        let grip_layer = container(corner_grip)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(grip_align)
            .align_y(Vertical::Bottom);

        let content_layer = container(col).width(Length::Fill).height(Length::Fill);

        stack![content_layer, grip_layer].into()
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn screenshot_oldest() -> Task<Message> {
    window::oldest().then(|id| match id {
        Some(id) => window::screenshot(id).map(Message::ScreenshotReady),
        None => Task::none(),
    })
}

fn apply_input_region(rects: Vec<crate::input_region::Rect>) -> Task<Message> {
    window::oldest().then(move |id| match id {
        Some(id) => {
            let rects = rects.clone();
            window::run(id, move |w| { apply_to_window(w, &rects); Message::PassthroughApplied(rects.len()) })
        }
        None => Task::none(),
    })
}

fn apply_to_window(w: &dyn iced::Window, rects: &[crate::input_region::Rect]) {
    let (Some(wh), Some(dh)) = (w.window_handle().ok(), w.display_handle().ok()) else { return; };
    let (raw_window_handle::RawWindowHandle::Wayland(sh), raw_window_handle::RawDisplayHandle::Wayland(dh)) =
        (wh.as_raw(), dh.as_raw()) else { return; };
    unsafe {
        crate::input_region::apply(sh.surface.as_ptr() as _, dh.display.as_ptr() as _, rects);
    }
}

fn asr_stream(addr: &String) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    let addr = addr.clone();
    Box::pin(iced::stream::channel::<Message>(100, move |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
        crate::service::asr::spawn(addr, move |upd| { let _ = sender.try_send(Message::Asr(upd)); });
    }))
}

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

/// Resolve the skin image via FileLoader (`SKIN::<rel>`), fall back to bundled bytes.
pub fn skin_source(rel: &str) -> Handle {
    let loader = fs::loader!();
    match loader.resolve(&format!("SKIN::{rel}")).filter(|p| p.exists()) {
        Some(p) => { eprintln!("[geek-familiar] skin: {}", p.display()); Handle::from_path(p.to_string_lossy().into_owned()) }
        None => { eprintln!("[geek-familiar] skin: {rel} not found, bundled fallback"); Handle::from_bytes(IDLE_PNG) }
    }
}
