//! Chat panel — resizable sections + edit buffer.

use crate::model::Message;
use crate::PetApp;
use iced::widget::{column, container, image, mouse_area, row, scrollable, text, text_editor, text_input};
use iced::{Background, Border, Color, ContentFit, Element, Length};

use super::style::{text_color as tc, text_dim, text_faint};

/// Build a section title bar (click to toggle collapse).
fn title_bar(idx: usize, title: &str, collapsed: bool, fs: f32) -> Element<'static, Message> {
    let arrow = if collapsed { "▶" } else { "▼" };
    mouse_area(
        container(
            row![
                text(format!("{arrow} {title}")).size(fs * 0.72).color(Color::from_rgb8(0x88, 0x88, 0x88)),
                iced::widget::Space::new().width(Length::Fill),
            ]
        )
        .padding([1, 4])
    )
    .on_press(Message::ToggleSection(idx))
    .into()
}

/// Build a draggable divider between sections.
fn divider(idx: usize, dragging: bool) -> Element<'static, Message> {
    mouse_area(
        container(iced::widget::Space::new().width(Length::Fill).height(3.0))
            .style(move |_theme| container::Style {
                background: Some(Background::Color(if dragging {
                    Color::from_rgba(0.3, 0.5, 0.9, 0.6)
                } else {
                    Color::from_rgba(0.3, 0.3, 0.4, 0.3)
                })),
                ..Default::default()
            })
    )
    .on_press(Message::SectionDragStart(idx))
    .interaction(iced::mouse::Interaction::ResizingVertically)
    .into()
}

/// The Chat tab panel: resizable sections + edit buffer.
pub fn chat_panel(state: &PetApp) -> Element<'_, Message> {
    let fs = state.font_size;
    let body = tc(&state.style);
    let dim = text_dim(&state.style);
    let faint = text_faint(&state.style);
    let mut rows: Vec<Element<Message>> = vec![];

    // Screenshot thumbnail
    if let Some(ref path) = state.screenshot_path {
        use iced::widget::image::Handle;
        let label = text("📸 Last screenshot").size(fs * 0.75).color(dim);
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

    // ── Build section content (wrapped in Fill scrollable) ──
    let mut section_rows: Vec<Element<Message>> = Vec::new();

    // ASR Section — single text_editor for cross-utterance selection
    section_rows.push(title_bar(0, "💬 ASR Messages", state.section_collapsed[0], fs));
    if !state.section_collapsed[0] {
        section_rows.push(
            scrollable(
                text_editor(&state.transcript)
                    .on_action(Message::TranscriptAction)
            )
            .height(Length::Fixed(state.section_heights[0]))
            .into()
        );
    }
    section_rows.push(divider(0, state.dragging_divider == Some(0)));

    // Clipboard Section
    section_rows.push(title_bar(1, "📋 Clipboard", state.section_collapsed[1], fs));
    if !state.section_collapsed[1] && !state.clipboard.is_empty() {
        let items: Vec<Element<Message>> = state.clipboard.iter().take(9).enumerate().map(|(i, s)| {
            let idx = text(format!("{}", i + 1)).size(fs * 0.75).color(dim);
            let d = dim; let b = body;
            let txt: Element<Message> = text_input("", s).size(fs * 0.85)
                .style(move |_t, _s| text_input::Style {
                    background: Background::Color(Color::TRANSPARENT),
                    border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
                    icon: Color::TRANSPARENT, placeholder: d,
                    value: b,
                    selection: Color::from_rgba(0.3, 0.5, 0.9, 0.5),
                }).into();
            container(row![idx, txt].spacing(6)).padding([2, 4]).width(Length::Fill).into()
        }).collect();
        section_rows.push(scrollable(column(items).spacing(1)).height(Length::Fixed(state.section_heights[1])).into());
    } else if !state.section_collapsed[1] {
        section_rows.push(text("no clipboard items").size(fs * 0.7).color(faint).into());
    }
    section_rows.push(divider(1, state.dragging_divider == Some(1)));

    // Status Section
    section_rows.push(title_bar(2, "📡 Status", state.section_collapsed[2], fs));
    if !state.section_collapsed[2] && !state.status_messages.is_empty() {
        let items: Vec<Element<Message>> = state.status_messages.iter().take(12).map(|msg| {
            text(msg.as_str()).size(fs * 0.7).color(dim).into()
        }).collect();
        section_rows.push(scrollable(column(items).spacing(0)).height(Length::Fixed(state.section_heights[2])).into());
    } else if !state.section_collapsed[2] {
        section_rows.push(text("no status messages").size(fs * 0.7).color(faint).into());
    }
    section_rows.push(divider(2, state.dragging_divider == Some(2)));

    // Wrap all sections in a Fill scrollable so edit buffer stays visible
    rows.push(
        scrollable(column(section_rows).spacing(0))
            .height(Length::Fill)
            .into()
    );

    // ── Edit buffer (always pinned at bottom) ──
    rows.push(
        text_editor(&state.scratchpad)
            .placeholder("compose text here…")
            .on_action(Message::ScratchpadEdit)
            .height(Length::Shrink)
            .into()
    );

    column(rows).spacing(4).padding(6).width(Length::Fill).into()
}
