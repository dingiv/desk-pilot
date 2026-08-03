//! Settings panel.

use crate::model::Message;
use crate::PetApp;
use iced::widget::{button, column, row, scrollable, text};
use iced::{Background, Border, Color, Element, Length};

use super::style::{self, text_color as tc, text_dim, text_faint};

pub fn settings_panel(state: &PetApp) -> Element<'_, Message> {
    let fs = state.font_size;
    let body = tc(&state.style);
    let dim = text_dim(&state.style);
    let faint = text_faint(&state.style);
    let mut rows: Vec<Element<Message>> = vec![
        text("⚙ 设置").color(body).size(fs * 1.1).into(),
    ];

    // ── Recording ──
    let (dot, label, color) = match state.asr.status() {
        crate::model::AsrStatus::Enabled => ("●", "录音中 (scout active)", Color::from_rgb8(0x4f, 0xef, 0x6f)),
        crate::model::AsrStatus::Disabled => ("◐", "录音暂停 (scout disabled)", Color::from_rgb8(0xef, 0xcf, 0x4f)),
        crate::model::AsrStatus::Disconnected => ("○", "未连接 aura", Color::from_rgb8(0xef, 0x6f, 0x6f)),
    };
    let rec_label = if state.asr.sse_connected { "⏸ 切换录音" } else { "(aura 未连接)" };
    rows.push(
        row![
            text(format!("{dot} {label}")).color(color).size(fs * 0.85),
            iced::widget::Space::new().width(Length::Fill),
            button(text(rec_label).size(fs * 0.8).color(body))
                .on_press(Message::ToggleRecording).padding([2, 8]),
        ].spacing(4).into()
    );

    // ── Aura daemon ──
    rows.push(text(format!("aura: {}", state.aura_addr))
        .color(dim).size(fs * 0.75).into());

    // ── Hotwords (placeholder) ──
    rows.push(
        text("🔤 Hotwords: (coming soon)")
            .color(dim).size(fs * 0.75).into()
    );

    // ── Recent corrections ──
    if !state.corrections.is_empty() {
        rows.push(text("📝 Recent corrections:").color(dim).size(fs * 0.8).into());
        let corr_items: Vec<Element<Message>> = state.corrections.iter().take(9).map(|(raw, fixed)| {
            let line = format!("\u{201c}{}\u{201d} → \u{201c}{}\u{201d}",
                if raw.chars().count() > 20 { format!("{}…", raw.chars().take(20).collect::<String>()) } else { raw.clone() },
                if fixed.chars().count() > 20 { format!("{}…", fixed.chars().take(20).collect::<String>()) } else { fixed.clone() },
            );
            text(line).color(faint).size(fs * 0.72).into()
        }).collect();
        rows.push(scrollable(column(corr_items).spacing(1)).height(Length::Fixed(fs * 8.0)).into());
    }

    // ── Aura health check ──
    rows.push(
        button(text("🔍 检查 aura 连接").size(fs * 0.8).color(body))
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
