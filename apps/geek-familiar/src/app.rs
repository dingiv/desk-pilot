//! geek-familiar — the desktop pet, rendered by iced.
//!
//! Architecture: **Model → Service → View**
//! - [`model`] — data types (config, state, messages)
//! - [`service`] — business logic (ASR SSE, aura HTTP, gnome extension)
//! - [`view`] — widget constructors and styling
//!
//! [`PetApp`] is the composition root: it owns the state, wires services
//! into subscriptions and tasks, and delegates rendering to the view layer.

use iced::widget::{column, container, image, mouse_area, row, stack, text, text_editor};
use iced::alignment::Vertical;
use iced::widget::image::Handle;
use iced::widget::svg::Handle as SvgHandle;
use iced::{Background, Border, Color, ContentFit, Degrees, Element, Gradient, Length, Subscription, Task, alignment, window};

pub(crate) use crate::model::{AsrState, DockingPreference, Message, Panel, StyleConfig};
pub(crate) use audio_aura_agent::agent::AuraAgent;
pub(crate) use crate::service::aura_client::{aura_stream, connect as connect_aura, play_audio};

/// Bundled fallback skin (used when FileLoader can't resolve the configured skin).
const IDLE_PNG: &[u8] = include_bytes!("../assets/skins/default/idle.png");
/// Resize-grip icon (four-pointed star, embedded SVG).
const RESIZOR_SVG: &[u8] = include_bytes!("../assets/skins/default/resizor.dio.svg");

const RESCAN_TICK :u64 = 5000;

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
    /// Clipboard history, newest first.
    pub(crate) clipboard: Vec<String>,
    pub(crate) scratchpad: text_editor::Content,
    /// Visual feedback: file drag hover over the window.
    pub(crate) drop_highlight: bool,
    /// Latest screenshot file path (for thumbnail display).
    pub(crate) screenshot_path: Option<String>,
    /// When Some(idx), turn `idx` is being edited in-place (correction mode).
    pub(crate) editing_turn: Option<u64>,
    /// The in-progress correction text for the turn being edited.
    pub(crate) correction_text: String,
    /// Submitted corrections (raw → corrected), newest first.
    pub(crate) corrections: Vec<(String, String)>,
    /// App-level status messages (errors, info, hints), newest first.
    pub(crate) status_messages: Vec<String>,
    /// Collapsed state for sections: [ASR, Clipboard, Status].
    pub(crate) section_collapsed: [bool; 3],
    /// Current heights for sections (pixels), persisted across collapse.
    pub(crate) section_heights: [f32; 3],
    /// When Some, the divider BELOW section `idx` is being dragged.
    pub(crate) dragging_divider: Option<usize>,
    /// Mouse Y at drag-start, for computing delta.
    drag_origin_y: f32,
    /// Previous mouse Y for delta computation.
    prev_mouse_y: f32,
    /// Selected ASR turn indices (for multi-select copy).
    pub(crate) selected_turns: Vec<u64>,
    /// Accumulating transcript — all utterances in one text_editor.
    pub(crate) transcript: text_editor::Content,
    /// Shared AuraAgent — owns the connection + state (background driver). Commands go through
    /// it; the iced subscription forwards its events.
    pub(crate) agent: std::sync::Arc<AuraAgent>,
}

impl PetApp {
    pub fn new(
        aura_addr: String, skin: Handle, token: String, font_size: f32,
        style: StyleConfig, window_bg: Option<String>, docking: DockingPreference,
        sprite_size: f32, sprite_filter: String,
    ) -> (Self, Task<Message>) {
        let init_status = format!("started — aura: {aura_addr}");
        let agent = connect_aura(&aura_addr)
            .unwrap_or_else(|e| { eprintln!("[geek-familiar] AuraAgent connect failed: {e}"); std::process::exit(1) });
        let app = PetApp {
            aura_addr, skin, token, font_size, sprite_size, sprite_filter, style, window_bg, docking,
            asr: AsrState::default(),
            active_panel: None, ime_input: String::new(), auto_move: false,
            clipboard: vec![],
            scratchpad: text_editor::Content::new(),
            drop_highlight: false,
            screenshot_path: None,
            editing_turn: None,
            correction_text: String::new(),
            corrections: Vec::new(),
            status_messages: vec![init_status],
            section_collapsed: [false, false, false],
            section_heights: [180.0, 120.0, 80.0],
            dragging_divider: None,
            drag_origin_y: 0.0,
            prev_mouse_y: 0.0,
            selected_turns: Vec::new(),
            transcript: text_editor::Content::new(),
            agent,
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
            Message::AuraEvent(ev) => {
                use audio_aura_agent::agent::AgentEvent::*;
                match ev {
                    // Control plane: snapshot refreshed → scout + corrections + hotwords.
                    StateChanged(view) => {
                        self.asr.sse_connected = true;
                        self.asr.scout_active = view.connected;
                        self.corrections = view.corrections.iter().map(|c| (c.raw.clone(), c.corrected.clone())).collect();
                        self.status_messages.retain(|m| !m.starts_with("🔤"));
                        self.status_messages.insert(0, format!("🔤 hotwords: {}", view.hotwords.join(", ")));
                        if self.status_messages.len() > 30 { self.status_messages.truncate(30); }
                    }
                    // Data plane: settled utterance → history + transcript.
                    TurnFinal(u) => {
                        let turn = crate::model::ConversationTurn {
                            seq: u.seq,
                            user_text: u.calibrated.clone(),
                            intent: u.intent.clone(),
                            reply: u.reply.clone(),
                        };
                        let existing = self.asr.history.iter_mut().find(|t| t.seq == u.seq);
                        if let Some(t) = existing { *t = turn; } else { self.asr.history.push(turn); }
                        if self.asr.history.len() > 20 {
                            self.asr.history.drain(0..self.asr.history.len() - 20);
                        }
                        let mut line = u.calibrated.clone();
                        if !u.reply.is_empty() {
                            line.push_str(&format!("\n   ◀ {}", u.reply));
                        }
                        let mut content = self.transcript.clone();
                        if !content.text().is_empty() { content = text_editor::Content::with_text(&format!("{}\n{}", content.text(), line)); }
                        else { content = text_editor::Content::with_text(&line); }
                        self.transcript = content;
                    }
                    // ① new Stage1 streaming fragment — raw partial (fast UI follow).
                    Interim { partial, .. } => self.asr.interim = partial,
                    // ② Stage2 corrected a batch — calibrated text wins over the raw partial.
                    CalibratedInterim { calibrated, .. } => self.asr.interim = calibrated,
                    // Connectivity changed (the agent probes /health itself).
                    ConnChanged(c) => self.asr.sse_connected = c == audio_aura_agent::AuraConn::Connected,
                    TurnCorrected(_) => {} // the pair already entered Stage2; UI shows via corrections
                }
            }
            // Placeholder for SSE connection status from the stream itself
            // (the old AsrUpdate variants aren't needed — snapshots are the source of truth)
            Message::HandshakeDone(ok) => {
                eprintln!("[geek-familiar] gnome-layer-ext handshake ok={ok}");
                if self.agent.conn() != audio_aura_agent::AuraConn::Connected {
                    eprintln!("[geek-familiar] aura daemon not reachable");
                }
            }
            Message::DragStarted => {
                return window::oldest().then(|id| match id {
                    Some(id) => window::drag(id),
                    None => Task::none(),
                });
            }
            Message::TabPressed(p) | Message::TabSelected(p) => {
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
                return apply_input_region(rects);
            }
            Message::PassthroughApplied(n) => eprintln!("[passthrough] applied {n} rect(s)"),
            Message::RescanTick => return screenshot_oldest(),
            Message::ToggleRecording => {
                if self.asr.sse_connected {
                    let enable = !self.asr.scout_active;
                    self.agent.set_connected(enable);
                }
            }
            Message::RecordingToggled => {
                // Status updated by AsrUpdate::Status event from SSE.
            }
            Message::ScratchpadEdit(action) => self.scratchpad.perform(action),
            Message::TranscriptAction(action) => self.transcript.perform(action),
            Message::FileDropped(path) => {
                eprintln!("[geek-familiar] file dropped: {path}");
                self.drop_highlight = false;
                self.scratchpad = text_editor::Content::with_text(&path);
            }
            Message::TakeScreenshot => {
                let path = "/tmp/geek-familiar-screenshot.png".to_string();
                return Task::perform(
                    async move {
                        let status = std::process::Command::new("gnome-screenshot")
                            .args(["-a", "-f", &path])
                            .status();
                        eprintln!("[geek-familiar] gnome-screenshot exit: {status:?}, file exists: {}", std::path::Path::new(&path).exists());
                        path
                    },
                    Message::ScreenshotSaved,
                );
            }
            Message::ScreenshotSaved(p) => {
                if std::path::Path::new(&p).exists() {
                    self.screenshot_path = Some(p.clone());
                    self.scratchpad = text_editor::Content::with_text(&p);
                    self.status_messages.insert(0, format!("📸 screenshot saved to {p}"));
                    if self.status_messages.len() > 30 { self.status_messages.truncate(30); }
                } else {
                    self.status_messages.insert(0, format!("⚠ screenshot failed — {p} not found"));
                    if self.status_messages.len() > 30 { self.status_messages.truncate(30); }
                }
            }
            Message::FileHovered => {
                eprintln!("[geek-familiar] file drag hover");
                self.drop_highlight = true;
            }
            Message::FileHoverLeft => {
                eprintln!("[geek-familiar] file drag left");
                self.drop_highlight = false;
            }
            Message::AsrContextMenu(idx) => {
                // Right-click on an ASR entry: copy its text to the scratchpad buffer
                // for editing, then dispatch or re-copy.
                if let Some(turn) = self.asr.history.get(idx as usize) {
                    self.scratchpad = text_editor::Content::with_text(&turn.user_text);
                }
            }
            Message::FixTurn(idx) => {
                // Enter inline edit mode: copy the turn's text into correction buffer.
                if let Some(turn) = self.asr.history.get(idx as usize) {
                    self.editing_turn = Some(idx);
                    self.correction_text = turn.user_text.clone();
                }
            }
            Message::PlayAudio(idx) => {
                eprintln!("[geek-familiar] play audio seq={idx}");
                play_audio(self.agent.clone(), idx);
            }
            Message::AudioPlayed(seq, ok) => {
                let msg = if ok { format!("🔊 audio seq={seq} played") } else { format!("⚠ audio seq={seq} failed") };
                self.status_messages.insert(0, msg);
                if self.status_messages.len() > 30 { self.status_messages.truncate(30); }
            }
            Message::CorrectionEdit(s) => self.correction_text = s,
            Message::SubmitCorrection(idx) => {
                if let Some(turn) = self.asr.history.get(idx as usize) {
                    let seq = turn.seq;
                    let raw = turn.user_text.clone();
                    let corrected = self.correction_text.clone();
                    self.corrections.insert(0, (raw.clone(), corrected.clone()));
                    if self.corrections.len() > 50 { self.corrections.truncate(50); }
                    self.agent.correct(seq, &raw, &corrected);
                    self.editing_turn = None;
                    self.correction_text.clear();
                }
            }
            Message::CancelEdit => {
                self.editing_turn = None;
                self.correction_text.clear();
            }
            Message::ClipboardPoll => return iced::clipboard::read().map(|opt| Message::ClipboardUpdate(opt.unwrap_or_default())),
            Message::ClipboardUpdate(s) => {
                if !s.is_empty() && self.clipboard.first().map_or(true, |last| last != &s) {
                    self.clipboard.insert(0, s);
                    if self.clipboard.len() > 50 { self.clipboard.truncate(50); }
                }
            }
            Message::ToggleSelectTurn(idx) => {
                if let Some(pos) = self.selected_turns.iter().position(|&x| x == idx) {
                    self.selected_turns.remove(pos);
                } else {
                    self.selected_turns.push(idx);
                }
            }
            Message::CopySelectedTurns => {
                if self.selected_turns.is_empty() { return Task::none(); }
                self.selected_turns.sort();
                let texts: Vec<String> = self.selected_turns.iter()
                    .filter_map(|&idx| self.asr.history.get(idx as usize))
                    .map(|t| t.user_text.clone())
                    .collect();
                let joined = texts.join("\n");
                return iced::clipboard::write(joined);
            }
            Message::ToggleSection(idx) => {
                let i = idx.min(2);
                self.section_collapsed[i] = !self.section_collapsed[i];
            }
            Message::SectionDragStart(idx) => {
                let i = idx.min(2);
                self.dragging_divider = Some(i);
                self.drag_origin_y = self.prev_mouse_y;
            }
            Message::SectionDragMove(y) => {
                self.prev_mouse_y = y;
                if let Some(i) = self.dragging_divider {
                    let delta = y - self.drag_origin_y;
                    self.drag_origin_y = y;
                    self.section_heights[i] = (self.section_heights[i] + delta).max(30.0).min(600.0);
                }
            }
            Message::SectionDragEnd => {
                self.dragging_divider = None;
            }
            Message::Quit => {
                return window::oldest().then(|id| match id {
                    Some(id) => window::close::<Message>(id),
                    None => Task::none(),
                });
            }
            Message::CheckHealth => {
                eprintln!("[geek-familiar] aura daemon {}", if self.agent.conn() == audio_aura_agent::AuraConn::Connected { "reachable" } else { "NOT reachable" });
            }
            Message::AppStatus(s) => {
                eprintln!("[geek-familiar] status: {s}");
                self.status_messages.insert(0, s);
                if self.status_messages.len() > 30 { self.status_messages.truncate(30); }
            }
            Message::HealthCheck(reachable) => {
                let msg = if reachable { "aura daemon reachable" } else { "aura daemon NOT reachable" };
                self.status_messages.insert(0, format!("🔍 {msg}"));
                if self.status_messages.len() > 30 { self.status_messages.truncate(30); }
                eprintln!("[geek-familiar] {msg}");
            }
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
            // AuraAgent: Hash = the daemon origin → id dedups by address; the fn pointer
            // reconstructs the stream per re-run.
            Subscription::run_with(self.agent.clone(), aura_stream).map(Message::AuraEvent),
            Subscription::run_with("rescan".to_string(), rescan_stream),
            Subscription::run_with(self.token.clone(), clipboard_stream),
            Subscription::run_with("clip_poll".to_string(), clipboard_poll_stream),
            iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Window(iced::window::Event::FileDropped(path)) => Some(Message::FileDropped(path.to_string_lossy().to_string())),
                iced::Event::Window(iced::window::Event::FileHovered(_)) => Some(Message::FileHovered),
                iced::Event::Window(iced::window::Event::FilesHoveredLeft) => Some(Message::FileHoverLeft),
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => Some(Message::SectionDragMove(position.y)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => Some(Message::SectionDragEnd),
                _ => None,
            }),
        ])
    }

    pub fn view(&self) -> Element<'_, Message> {
        let style = self.style.clone();
        let fs = self.font_size;
        let sprite = image(self.skin.clone()).width(self.sprite_size).height(self.sprite_size)
            .content_fit(ContentFit::Contain)
            .filter_method(if self.sprite_filter == "nearest" { iced::widget::image::FilterMethod::Nearest } else { iced::widget::image::FilterMethod::Linear });
        let mut tab_bar = iced_aw::TabBar::new(|tab_id| Message::TabSelected(tab_id))
            .push(Panel::Chat, iced_aw::TabLabel::Text("💬".into()))
            .push(Panel::Settings, iced_aw::TabLabel::Text("⚙".into()))
            .tab_width(Length::Fixed(40.0))
            .height(34.0)
            .padding(0)
            .width(Length::Shrink)
            .style({
                let s = style.clone();
                let neutral = Color::from_rgba(s.pill_neutral[0], s.pill_neutral[1], s.pill_neutral[2], s.pill_neutral[3]);
                // Build active gradient from config
                let gradient_bg: Background = s.pill_accent_gradient.as_ref().map_or_else(
                    || {
                        let [r, g, b, a] = s.pill_accent;
                        Background::Color(Color::from_rgba(r, g, b, a))
                    },
                    |g| {
                        if g.len() >= 9 {
                            Background::Gradient(Gradient::Linear(iced_core::gradient::Linear::new(Degrees(g[0]))
                                .add_stop(0.0, Color::from_rgba(g[1], g[2], g[3], g[4]))
                                .add_stop(1.0, Color::from_rgba(g[5], g[6], g[7], g[8]))))
                        } else {
                            let [r, g2, b, a] = s.pill_accent;
                            Background::Color(Color::from_rgba(r, g2, b, a))
                        }
                    },
                );
                move |_theme, status| {
                    use iced_aw::style::status::Status;
                    let is_active = matches!(status, Status::Active);
                    let is_hovered = matches!(status, Status::Hovered);
                    iced_aw::style::tab_bar::Style {
                        background: None,
                        tab_label_background: if is_active || is_hovered {
                            gradient_bg.clone()
                        } else {
                            Background::Color(neutral)
                        },
                        tab_label_border_color: if is_hovered && !is_active {
                            Color::from_rgba(1.0, 1.0, 1.0, 0.35)
                        } else {
                            Color::TRANSPARENT
                        },
                        tab_label_border_width: if is_hovered && !is_active { 1.5 } else { 0.0 },
                        tab_border_radius: 14.0.into(),
                        text_color: Color::WHITE,
                        icon_color: Color::WHITE,
                        ..iced_aw::style::tab_bar::Style::default()
                    }
                }
            });
        if let Some(active) = self.active_panel {
            tab_bar = tab_bar.set_active_tab(&active);
        }
        let (h_align, grip_align) = match self.docking {
            DockingPreference::Left => (iced::alignment::Horizontal::Left, iced::alignment::Horizontal::Right),
            DockingPreference::Right => (iced::alignment::Horizontal::Right, iced::alignment::Horizontal::Left),
        };
        let spacer_width = if self.docking == DockingPreference::Right { Length::Fill } else { Length::Fixed(0.0) };
        let dock = row![
            crate::view::drag_button(fs, style.clone()),
            crate::view::asr_dock_button(self, style.clone()),
            mouse_area(
                container(text("📷").size(16.0))
                    .width(32.0).height(28.0)
                    .align_x(iced::alignment::Horizontal::Center).align_y(iced::alignment::Vertical::Center)
                    .style({ let s = style.clone(); move |_theme| crate::view::pill_style(&s, false) }),
            ).on_press(Message::TakeScreenshot).interaction(iced::mouse::Interaction::Pointer),
            iced::widget::Space::new().width(spacer_width),
            tab_bar,
        ].spacing(4);
        let mut col = column![sprite, dock].align_x(h_align).spacing(6);
        if let Some(panel) = self.active_panel {
            let body: Element<Message> = match panel {
                Panel::Chat => crate::view::chat_panel(self),
                Panel::Settings => crate::view::settings_panel(self),
            };
            col = col.push(container(body).width(Length::Fill).height(Length::Fill).style({ let s = style.clone(); move |_theme| crate::view::card_style(&s, 0.92) }));
        }
        let corner_grip = mouse_area(
            container(iced::widget::svg::Svg::new(self.resize_handle.clone()).width(12).height(20)).padding([2, 4]),
        ).on_press(Message::DiagonalResizeStart).interaction(iced::mouse::Interaction::ResizingDiagonallyUp);
        let grip_layer = container(corner_grip).width(Length::Fill).height(Length::Fill).align_x(grip_align).align_y(Vertical::Bottom);
        let mut content_layer = container(col).width(Length::Fill).height(Length::Fill);
        if self.drop_highlight {
            content_layer = content_layer.style(|_theme| container::Style {
                background: Some(Background::Color(Color::from_rgba(0.2, 0.3, 0.6, 0.3))),
                border: Border { color: Color::from_rgba(0.3, 0.5, 0.9, 0.7), width: 2.0, radius: 10.0.into() },
                ..Default::default()
            });
        }
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
    unsafe { crate::input_region::apply(sh.surface.as_ptr() as _, dh.display.as_ptr() as _, rects); }
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

fn clipboard_stream(token: &String) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    let token = token.clone();
    Box::pin(iced::stream::channel::<Message>(8, move |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
        crate::service::gnome_ext::subscribe_clipboard(token, move |text| {
            let _ = sender.try_send(Message::ClipboardUpdate(text));
        });
    }))
}

fn clipboard_poll_stream(_id: &String) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    Box::pin(iced::stream::channel::<Message>(4, |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(3000));
                let _ = sender.try_send(Message::ClipboardPoll);
            }
        });
    }))
}

pub fn skin_source(rel: &str) -> Handle {
    let loader = fs::loader!();
    match loader.resolve(&format!("SKIN::{rel}")).filter(|p| p.exists()) {
        Some(p) => { eprintln!("[geek-familiar] skin: {}", p.display()); Handle::from_path(p.to_string_lossy().into_owned()) }
        None => { eprintln!("[geek-familiar] skin: {rel} not found, bundled fallback"); Handle::from_bytes(IDLE_PNG) }
    }
}
