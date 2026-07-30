//! View layer — widget constructors and styling helpers.
//!
//! Pure rendering; no business logic.  Functions return [`Element`] trees
//! consumed by [`PetApp::view`].

use crate::model::{Message, Panel, StyleConfig};
use iced::widget::{button, column, container, image, mouse_area, row, scrollable, text, text_input};
use iced::{
    alignment, mouse, Background, Border, Color, ContentFit, Degrees, Element, Gradient,
    Length, Shadow, Vector,
};
use iced_core::gradient::Linear;

// ── Styling helpers ───────────────────────────────────────────────────────────

/// A semi-transparent rounded-card container style with an optional shadow.
pub fn card_style(style: &StyleConfig, alpha: f32) -> container::Style {
    let [r, g, b, a] = style.card_bg;
    let bg_alpha = (a * alpha).min(1.0);
    let [sr, sg, sb, sa] = style.shadow_color;
    container::Style {
        background: Some(Background::Color(Color::from_rgba(r, g, b, bg_alpha))),
        border: Border {
            color: Color::from_rgba(0.4, 0.4, 0.5, bg_alpha * 0.8),
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(sr, sg, sb, sa),
            offset: Vector::new(
                style.shadow_offset.map_or(0.0, |o| o[0]),
                style.shadow_offset.map_or(2.0, |o| o[1]),
            ),
            blur_radius: style.shadow_blur.unwrap_or(0.0),
        },
        ..Default::default()
    }
}

/// Pill style for a dock button. Active buttons get a gradient; inactive get a solid colour.
pub fn pill_style(style: &StyleConfig, active: bool) -> container::Style {
    let background = if active {
        style.pill_accent_gradient.as_ref().map_or_else(
            || {
                let [r, g, b, a] = style.pill_accent;
                Background::Color(Color::from_rgba(r, g, b, a))
            },
            |g| {
                // g[0]=angle°, g[1..5]=start rgba, g[5..9]=end rgba
                if g.len() >= 9 {
                    let angle = g[0];
                    let start = Color::from_rgba(g[1], g[2], g[3], g[4]);
                    let end = Color::from_rgba(g[5], g[6], g[7], g[8]);
                    Background::Gradient(Gradient::Linear(
                        Linear::new(Degrees(angle)).add_stop(0.0, start).add_stop(1.0, end),
                    ))
                } else {
                    let [r, gr, b, a] = style.pill_accent;
                    Background::Color(Color::from_rgba(r, gr, b, a))
                }
            },
        )
    } else {
        let [r, g, b, a] = style.pill_neutral;
        Background::Color(Color::from_rgba(r, g, b, a))
    };
    container::Style {
        background: Some(background),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 14.0.into() },
        ..Default::default()
    }
}

/// Parse `"r, g, b, a"` → `Color`.  Empty → `None`.
pub fn parse_bg(s: &str) -> Option<Color> {
    let parts: Vec<f32> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    (parts.len() >= 4).then(|| Color::from_rgba(parts[0], parts[1], parts[2], parts[3]))
}

// ── Dock buttons ─────────────────────────────────────────────────────────────

pub fn drag_button(_fs: f32, style: StyleConfig) -> Element<'static, Message> {
    mouse_area(
        container(text("⠿").size(16.0).color(Color::from_rgb8(0xcc, 0xcc, 0xcc)))
            .width(32.0).height(28.0)
            .align_x(alignment::Horizontal::Center)
            .align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, false) }),
    )
    .on_press(Message::DragStarted)
    .interaction(mouse::Interaction::Grab)
    .into()
}

pub fn recording_button(recording: bool, _fs: f32, style: StyleConfig) -> Element<'static, Message> {
    let label = if recording { "🎙" } else { "⏸" };
    mouse_area(
        container(text(label).size(16.0).color(Color::WHITE))
            .width(32.0).height(28.0)
            .align_x(alignment::Horizontal::Center)
            .align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, recording) }),
    )
    .on_press(Message::ToggleRecording)
    .interaction(mouse::Interaction::Pointer)
    .into()
}

pub fn dock_button(icon: &'static str, panel: Panel, active: bool, _fs: f32, style: StyleConfig) -> Element<'static, Message> {
    mouse_area(
        container(text(icon).size(16.0).color(Color::WHITE))
            .width(32.0).height(28.0)
            .align_x(alignment::Horizontal::Center)
            .align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, active) }),
    )
    .on_press(Message::TabPressed(panel))
    .interaction(mouse::Interaction::Pointer)
    .into()
}

// ── Tab panels ───────────────────────────────────────────────────────────────

use crate::PetApp;

/// The Chat tab panel: ASR status + live interim + scrollable conversation thread
/// + text input.
pub fn chat_panel(state: &PetApp) -> Element<'_, Message> {
    let fs = state.font_size;
    let (dot, label, color) = if state.asr.connected {
        ("●", "ASR live", Color::from_rgb8(0x4f, 0xef, 0x6f))
    } else {
        ("○", "ASR off", Color::from_rgb8(0xef, 0x6f, 0x6f))
    };

    let mut rows: Vec<Element<Message>> = vec![text(format!("{dot} {label}")).color(color).into()];

    if !state.asr.interim.is_empty() {
        rows.push(text(&state.asr.interim).color(Color::from_rgb8(0xaa, 0xaa, 0xaa)).into());
    }

    if !state.asr.history.is_empty() {
        let items: Vec<Element<Message>> = state.asr.history.iter()
            .map(|turn| {
                let intent_badge = if turn.intent.is_empty() || turn.intent == "chat" {
                    text("💬 chat").size(fs * 0.7).color(Color::from_rgb8(0x66, 0xaa, 0xff))
                } else {
                    text(format!("⚡ {}", turn.intent)).size(fs * 0.7).color(Color::from_rgb8(0xff, 0x88, 0x44))
                };
                let mut card = column![
                    text(&turn.user_text).color(Color::WHITE).size(fs * 0.9),
                    intent_badge,
                ].spacing(2);
                if !turn.reply.is_empty() {
                    card = card.push(
                        text(format!("◀ {}", turn.reply))
                            .color(Color::from_rgb8(0xaa, 0xaa, 0xaa))
                            .size(fs * 0.85),
                    );
                }
                container(card).padding([4, 8])
                    .style(|_theme| card_style(&state.style, 0.3))
                    .width(Length::Fill)
                    .into()
            })
            .collect();
        rows.push(
            scrollable(column(items).spacing(4))
                .anchor_bottom()
                .height(Length::Fill)
                .into(),
        );
    }

    rows.push(
        text_input("type a message…", &state.ime_input)
            .on_input(Message::ImeInput)
            .into(),
    );

    column(rows).spacing(4).padding(6)
        .width(Length::Fill)
        .into()
}

/// The Settings tab panel.
pub fn settings_panel(state: &PetApp) -> Element<'_, Message> {
    column![
        text("设置").color(Color::WHITE).size(state.font_size),
        button(text(if state.auto_move { "☑ 自动游走" } else { "☐ 自动游走" }).color(Color::WHITE))
            .on_press(Message::ToggleAutoMove)
            .padding([2, 6]),
        text(format!("aura: {}", state.aura_addr))
            .color(Color::from_rgb8(0x88, 0x88, 0x88))
            .size(state.font_size * 0.8),
        iced::widget::Space::new().height(6.0),
        button(text("退出程序").color(Color::from_rgb8(0xef, 0x6f, 0x6f)).size(state.font_size * 0.85))
            .on_press(Message::Quit)
            .padding([3, 10])
            .style(|_theme, _status| iced::widget::button::Style {
                background: Some(Background::Color(Color::from_rgba(0.25, 0.10, 0.10, 0.8))),
                border: Border { color: Color::from_rgba(0.5, 0.2, 0.2, 0.5), width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }),
    ]
    .spacing(4).padding(6)
    .width(Length::Fill)
    .into()
}
