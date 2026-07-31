//! View layer — widget constructors and styling helpers.

use crate::model::{Message, Panel, StyleConfig};
use crate::PetApp;
use iced::widget::{button, column, container, image, mouse_area, row, scrollable, text, text_editor, text_input};

/// A read-only, transparent text_input that looks like plain text but allows
/// text selection (Ctrl+C).
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
        container(text("⠿").size(18.0).color(Color::from_rgb8(0xcc, 0xcc, 0xcc)))
            .width(32.0).height(28.0).align_x(alignment::Horizontal::Center).align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, false) }),
    ).on_press(Message::DragStarted).interaction(mouse::Interaction::Grab).into()
}

pub fn dock_button(icon: &'static str, panel: Panel, active: bool, _fs: f32, style: StyleConfig) -> Element<'static, Message> {
    mouse_area(
        container(text(icon).size(18.0).color(Color::WHITE))
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
            text(format!("{dot} {label}")).color(color).size(fs),
            iced::widget::Space::new().width(Length::Fill),
            button(text("📷")).on_press(Message::TakeScreenshot).padding([2, 6]),
            button(text(rec_label).size(fs).color(Color::WHITE))
                .on_press(Message::ToggleRecording).padding([2, 6]),
        ]
        .spacing(4)
        .into(),
    ];

    // Screenshot thumbnail
    if let Some(ref path) = state.screenshot_path {
        use iced::widget::image::Handle;
        let label = text("📸 Last screenshot").size(fs * 0.75).color(Color::from_rgb8(0x88, 0x88, 0x88));
        let thumb = if std::path::Path::new(path).exists() {
            image(Handle::from_path(path))
                .width(160).height(100)
                .content_fit(ContentFit::Contain)
                .into()
        } else {
            text("(file not found)").color(Color::from_rgb8(0xff, 0x88, 0x44)).into()
        };
        rows.push(label.into());
        rows.push(thumb);
    }

    // ── Conversation thread ──
    if !state.asr.history.is_empty() {
        let last = state.asr.history.len() - 1;
        let items: Vec<Element<Message>> = state.asr.history.iter().enumerate().map(|(idx, turn)| {
            let is_last = idx == last;
            let idx_u64 = idx as u64;

            // ── User utterance (or inline editor when correcting) ──
            let editing = state.editing_turn == Some(idx_u64);
            let user_line: Element<Message> = if editing {
                // Inline correction mode: editable text_input + submit/cancel
                row![
                    text_input("correct the transcript…", &state.correction_text)
                        .on_input(Message::CorrectionEdit)
                        .on_submit(Message::SubmitCorrection(idx_u64))
                        .size(fs * 0.9)
                        .width(Length::Fill),
                    button(text("✓").size(fs * 0.8).color(Color::from_rgb8(0x4f, 0xef, 0x6f)))
                        .on_press(Message::SubmitCorrection(idx_u64))
                        .padding([1, 4]),
                    button(text("✗").size(fs * 0.8).color(Color::from_rgb8(0xef, 0x6f, 0x6f)))
                        .on_press(Message::CancelEdit)
                        .padding([1, 4]),
                ].spacing(2).into()
            } else {
                let display_text = if is_last && !state.asr.interim.is_empty() {
                    format!("{}\n— {} —", turn.user_text, state.asr.interim)
                } else {
                    turn.user_text.clone()
                };
                text(display_text).color(Color::WHITE).size(fs * 0.9).into()
            };

            // ── Intent pill ──
            let (intent_icon, intent_color) = match turn.intent.as_str() {
                "chat" => ("💬 chat", Color::from_rgba(0.25, 0.50, 0.85, 0.85)),
                "task" => ("⚡ task", Color::from_rgba(0.85, 0.55, 0.20, 0.85)),
                _ => ("❓ ?", Color::from_rgba(0.50, 0.50, 0.50, 0.70)),
            };
            let intent_pill = container(text(intent_icon).size(fs * 0.7).color(Color::WHITE))
                .padding([1, 6])
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(intent_color)),
                    border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 8.0.into() },
                    ..Default::default()
                });

            let mut card_rows: Vec<Element<Message>> = vec![user_line.into(), intent_pill.into()];

            // ── LLM reply (if present) ──
            if !turn.reply.is_empty() {
                let reply_text = text(format!("◀ {}", turn.reply))
                    .color(Color::from_rgba(0.70, 0.70, 0.78, 0.90))
                    .size(fs * 0.82);
                card_rows.push(reply_text.into());
            }

            // ── Action row ──
            let actions = row![
                button(text("✏ fix").size(fs * 0.7))
                    .on_press(Message::FixTurn(idx_u64))
                    .padding([1, 5]),
                button(text("🔊").size(fs * 0.7))
                    .on_press(Message::PlayAudio(idx_u64))
                    .padding([1, 5]),
            ].spacing(4);
            card_rows.push(actions.into());

            // Wrap the whole card in a subtle container with right-click support
            let card = container(column(card_rows).spacing(3))
                .padding([6, 8])
                .width(Length::Fill);
            mouse_area(card)
                .on_right_press(Message::AsrContextMenu(idx_u64))
                .into()
        }).collect();
        rows.push(scrollable(column(items).spacing(4)).anchor_bottom().height(Length::Fill).into());
    } else {
        // Push bottom sections down when there are no ASR messages.
        rows.push(iced::widget::Space::new().height(Length::Fill).into());
    }

    // ── 📋 Clipboard history (selectable plain text) ──
    let hdr = text("📋 Clipboard").size(fs * 0.85).color(Color::from_rgb8(0x88, 0x88, 0x88));
    let items: Vec<Element<Message>> = state.clipboard.iter().take(9).enumerate().map(|(i, s)| {
        let short = if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s.clone() };
        let idx = text(format!("{}", i + 1)).size(fs * 0.75).color(Color::from_rgb8(0x66, 0x66, 0x66));
            let txt: Element<Message> = text_input("", &short).size(fs * 0.85)
            .style(|_t, _s| text_input::Style { background: Background::Color(Color::TRANSPARENT), border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() }, icon: Color::TRANSPARENT, placeholder: Color::from_rgb8(0x88, 0x88, 0x88), value: Color::from_rgb8(0xcc, 0xcc, 0xcc), selection: Color::from_rgba(0.3, 0.5, 0.9, 0.5) })
            .into();
        container(row![idx, txt].spacing(6)).padding([2, 4]).width(Length::Fill).into()
    }).collect();
    rows.push(hdr.into());
    rows.push(scrollable(column(items).spacing(1)).height(Length::Fixed(fs * 8.0)).into());

    // ── Status messages (app-level errors, hints, info) ──
    if !state.status_messages.is_empty() {
        let status_hdr = text("📡 Status").size(fs * 0.75).color(Color::from_rgb8(0x66, 0x66, 0x66));
        let status_items: Vec<Element<Message>> = state.status_messages.iter().take(12).map(|msg| {
            text(msg.as_str()).size(fs * 0.7).color(Color::from_rgb8(0x99, 0x99, 0x99)).into()
        }).collect();
        rows.push(status_hdr.into());
        rows.push(scrollable(column(status_items).spacing(0))
            .height(Length::Fixed(fs * 5.0))
            .into());
    }

    // ── Scratchpad buffer (pinned to bottom, auto-grows) ──
    rows.push(
        text_editor(&state.scratchpad)
            .placeholder("compose text here…")
            .on_action(Message::ScratchpadEdit)
            .into()
    );

    column(rows).spacing(4).padding(6).width(Length::Fill).into()
}

/// The Settings tab panel.
pub fn settings_panel(state: &PetApp) -> Element<'_, Message> {
    let fs = state.font_size;
    let mut rows: Vec<Element<Message>> = vec![
        text("⚙ 设置").color(Color::WHITE).size(fs * 1.1).into(),
    ];

    // ── Recording ──
    let (dot, label, color) = if state.asr.connected {
        ("●", "录音中 (scout connected)", Color::from_rgb8(0x4f, 0xef, 0x6f))
    } else {
        ("○", "录音已停止", Color::from_rgb8(0xef, 0x6f, 0x6f))
    };
    let rec_label = if state.asr.connected { "⏸ 停止录音" } else { "🎙 开始录音" };
    rows.push(
        row![
            text(format!("{dot} {label}")).color(color).size(fs * 0.85),
            iced::widget::Space::new().width(Length::Fill),
            button(text(rec_label).size(fs * 0.8).color(Color::WHITE))
                .on_press(Message::ToggleRecording).padding([2, 8]),
        ].spacing(4).into()
    );

    // ── Aura daemon ──
    rows.push(text(format!("aura: {}", state.aura_addr))
        .color(Color::from_rgb8(0x88, 0x88, 0x88)).size(fs * 0.75).into());

    // ── Hotwords (placeholder — needs GET /context) ──
    rows.push(
        text("🔤 Hotwords: (coming soon)")
            .color(Color::from_rgb8(0x66, 0x66, 0x66)).size(fs * 0.75).into()
    );

    // ── Recent corrections ──
    if !state.corrections.is_empty() {
        rows.push(text("📝 Recent corrections:").color(Color::from_rgb8(0x88, 0x88, 0x88)).size(fs * 0.8).into());
        let corr_items: Vec<Element<Message>> = state.corrections.iter().take(9).map(|(raw, fixed)| {
            let line = format!("\u{201c}{}\u{201d} → \u{201c}{}\u{201d}",
                if raw.chars().count() > 20 { format!("{}…", raw.chars().take(20).collect::<String>()) } else { raw.clone() },
                if fixed.chars().count() > 20 { format!("{}…", fixed.chars().take(20).collect::<String>()) } else { fixed.clone() },
            );
            text(line).color(Color::from_rgb8(0xaa, 0xaa, 0xaa)).size(fs * 0.72).into()
        }).collect();
        rows.push(scrollable(column(corr_items).spacing(1)).height(Length::Fixed(fs * 8.0)).into());
    }

    // ── Aura health check ──
    rows.push(
        button(text("🔍 检查 aura 连接").size(fs * 0.8).color(Color::WHITE))
            .on_press(Message::CheckHealth)
            .padding([2, 8])
            .into()
    );

    rows.push(iced::widget::Space::new().height(4.0).into());

    // ── Quit ──
    rows.push(
        button(text("退出程序").color(Color::from_rgb8(0xef, 0x6f, 0x6f)).size(fs * 0.85))
            .on_press(Message::Quit).padding([3, 10])
            .style(|_, _| iced::widget::button::Style {
                background: Some(Background::Color(Color::from_rgba(0.25, 0.10, 0.10, 0.8))),
                border: Border { color: Color::from_rgba(0.5, 0.2, 0.2, 0.5), width: 1.0, radius: 6.0.into() },
                ..Default::default()
            })
            .into()
    );

    column(rows).spacing(5).padding(8).width(Length::Fill).into()
}
