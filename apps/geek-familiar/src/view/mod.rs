//! View layer — widget constructors and styling helpers.

use crate::model::{Message, Panel, StyleConfig};
use crate::PetApp;
use iced::widget::{button, column, container, image, mouse_area, row, scrollable, text, text_input};
use iced::{
    alignment, mouse, Background, Border, Color, ContentFit, Degrees, Element, Gradient,
    Length, Shadow, Vector,
};
use iced_core::gradient::Linear;

// ── Styling helpers ───────────────────────────────────────────────────────────

pub fn card_style(style: &StyleConfig, alpha: f32) -> container::Style {
    let [r, g, b, a] = style.card_bg;
    let bg_alpha = (a * alpha).min(1.0);
    let [sr, sg, sb, sa] = style.shadow_color;
    container::Style {
        background: Some(Background::Color(Color::from_rgba(r, g, b, bg_alpha))),
        border: Border { color: Color::from_rgba(0.4, 0.4, 0.5, bg_alpha * 0.8), width: 1.0, radius: 10.0.into() },
        shadow: Shadow {
            color: Color::from_rgba(sr, sg, sb, sa),
            offset: Vector::new(style.shadow_offset.map_or(0.0, |o| o[0]), style.shadow_offset.map_or(2.0, |o| o[1])),
            blur_radius: style.shadow_blur.unwrap_or(0.0),
        },
        ..Default::default()
    }
}

pub fn pill_style(style: &StyleConfig, active: bool) -> container::Style {
    let background = if active {
        style.pill_accent_gradient.as_ref().map_or_else(
            || { let [r, g, b, a] = style.pill_accent; Background::Color(Color::from_rgba(r, g, b, a)) },
            |g| {
                if g.len() >= 9 {
                    Background::Gradient(Gradient::Linear(Linear::new(Degrees(g[0]))
                        .add_stop(0.0, Color::from_rgba(g[1], g[2], g[3], g[4]))
                        .add_stop(1.0, Color::from_rgba(g[5], g[6], g[7], g[8]))))
                } else { let [r, gr, b, a] = style.pill_accent; Background::Color(Color::from_rgba(r, gr, b, a)) }
            })
    } else { let [r, g, b, a] = style.pill_neutral; Background::Color(Color::from_rgba(r, g, b, a)) };
    container::Style { background: Some(background), border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 14.0.into() }, ..Default::default() }
}

pub fn parse_bg(s: &str) -> Option<Color> {
    let parts: Vec<f32> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    (parts.len() >= 4).then(|| Color::from_rgba(parts[0], parts[1], parts[2], parts[3]))
}

// ── Dock buttons ─────────────────────────────────────────────────────────────

pub fn drag_button(_fs: f32, style: StyleConfig) -> Element<'static, Message> {
    mouse_area(
        container(text("⠿").size(16.0).color(Color::from_rgb8(0xcc, 0xcc, 0xcc)))
            .width(32.0).height(28.0).align_x(alignment::Horizontal::Center).align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, false) }),
    ).on_press(Message::DragStarted).interaction(mouse::Interaction::Grab).into()
}

pub fn dock_button(icon: &'static str, panel: Panel, active: bool, _fs: f32, style: StyleConfig) -> Element<'static, Message> {
    mouse_area(
        container(text(icon).size(16.0).color(Color::WHITE))
            .width(32.0).height(28.0).align_x(alignment::Horizontal::Center).align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, active) }),
    ).on_press(Message::TabPressed(panel)).interaction(mouse::Interaction::Pointer).into()
}

// ── Tab panels ───────────────────────────────────────────────────────────────

/// The Chat tab panel: recording toggle + ASR status + conversation + clipboard + input.
pub fn chat_panel(state: &PetApp) -> Element<'_, Message> {
    let fs = state.font_size;
    let (dot, label, color) = if state.asr.connected {
        ("●", "ASR live", Color::from_rgb8(0x4f, 0xef, 0x6f))
    } else {
        ("○", "ASR off", Color::from_rgb8(0xef, 0x6f, 0x6f))
    };

    // Status bar with recording toggle
    let rec_label = if state.asr.connected { "🎙 Stop" } else { "⏸ Start" };
    let mut rows: Vec<Element<Message>> = vec![
        row![
            text(format!("{dot} {label}")).color(color).size(fs * 0.85),
            iced::widget::Space::new().width(Length::Fill),
            button(text(rec_label).size(fs * 0.75).color(Color::WHITE))
                .on_press(Message::ToggleRecording).padding([2, 6]),
        ]
        .spacing(4)
        .into(),
    ];

    if !state.asr.interim.is_empty() {
        rows.push(text(&state.asr.interim).color(Color::from_rgb8(0xaa, 0xaa, 0xaa)).into());
    }

    if !state.asr.history.is_empty() {
        let items: Vec<Element<Message>> = state.asr.history.iter().map(|turn| {
            let badge = if turn.intent.is_empty() || turn.intent == "chat" {
                text("💬 chat").size(fs * 0.7).color(Color::from_rgb8(0x66, 0xaa, 0xff))
            } else { text(format!("⚡ {}", turn.intent)).size(fs * 0.7).color(Color::from_rgb8(0xff, 0x88, 0x44)) };
            let mut card = column![text(&turn.user_text).color(Color::WHITE).size(fs * 0.9), badge].spacing(2);
            if !turn.reply.is_empty() {
                card = card.push(text(format!("◀ {}", turn.reply)).color(Color::from_rgb8(0xaa, 0xaa, 0xaa)).size(fs * 0.85));
            }
            container(card).padding([4, 8]).style(|_| card_style(&state.style, 0.3)).width(Length::Fill).into()
        }).collect();
        rows.push(scrollable(column(items).spacing(4)).anchor_bottom().height(Length::Fill).into());
    }

    // ── Clipboard history (always visible, top 4 + scrollable) ──
    let hdr = text("📋 Clipboard").size(fs * 0.7).color(Color::from_rgb8(0x88, 0x88, 0x88));
    let items: Vec<Element<Message>> = state.clipboard.iter().take(9).enumerate().map(|(i, s)| {
        let short = if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s.clone() };
        let idx = text(format!("{}", i + 1)).size(fs * 0.6).color(Color::from_rgb8(0x66, 0x66, 0x66));
        let txt = text(short).size(fs * 0.7).color(Color::from_rgb8(0xcc, 0xcc, 0xcc));
        container(row![idx, txt].spacing(6)).padding([1, 4]).width(Length::Fill).into()
    }).collect();
    rows.push(hdr.into());
    rows.push(scrollable(column(items).spacing(1)).height(Length::Fixed(fs * 8.0)).into());

    rows.push(text_input("type a message…", &state.ime_input).on_input(Message::ImeInput).into());
    column(rows).spacing(4).padding(6).width(Length::Fill).into()
}

/// The Settings tab panel.
pub fn settings_panel(state: &PetApp) -> Element<'_, Message> {
    column![
        text("设置").color(Color::WHITE).size(state.font_size),
        button(text(if state.auto_move { "☑ 自动游走" } else { "☐ 自动游走" }).color(Color::WHITE))
            .on_press(Message::ToggleAutoMove).padding([2, 6]),
        text(format!("aura: {}", state.aura_addr)).color(Color::from_rgb8(0x88, 0x88, 0x88)).size(state.font_size * 0.8),
        iced::widget::Space::new().height(6.0),
        button(text("退出程序").color(Color::from_rgb8(0xef, 0x6f, 0x6f)).size(state.font_size * 0.85))
            .on_press(Message::Quit).padding([3, 10])
            .style(|_, _| iced::widget::button::Style {
                background: Some(Background::Color(Color::from_rgba(0.25, 0.10, 0.10, 0.8))),
                border: Border { color: Color::from_rgba(0.5, 0.2, 0.2, 0.5), width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }),
    ].spacing(4).padding(6).width(Length::Fill).into()
}
