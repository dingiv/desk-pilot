//! Chat panel — resizable sections + edit buffer.

use crate::model::Message;
use crate::PetApp;
use iced::widget::{button, column, container, image, mouse_area, row, scrollable, text, text_editor, text_input};
use iced::{Background, Border, Color, ContentFit, Element, Length};
use iced_aw::ContextMenu;

use super::style::{self, text_color as tc, text_dim, text_faint, text_subtle};

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
    let subtle = text_subtle(&state.style);
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

    // ASR Section
    section_rows.push(title_bar(0, "💬 ASR Messages", state.section_collapsed[0], fs));
    if !state.section_collapsed[0] && !state.asr.history.is_empty() {
        let last = state.asr.history.len() - 1;
        let items: Vec<Element<Message>> = state.asr.history.iter().enumerate().map(|(idx, turn)| {
            let is_last = idx == last;
            let idx_u64 = idx as u64;

            let editing = state.editing_turn == Some(idx_u64);
            let user_line: Element<Message> = if editing {
                row![
                    text_input("correct the transcript…", &state.correction_text)
                        .on_input(Message::CorrectionEdit)
                        .on_submit(Message::SubmitCorrection(idx_u64))
                        .size(fs * 0.9).width(Length::Fill),
                    button(text("✓").size(fs * 0.8).color(Color::from_rgb8(0x4f, 0xef, 0x6f)))
                        .on_press(Message::SubmitCorrection(idx_u64)).padding([1, 4]),
                    button(text("✗").size(fs * 0.8).color(Color::from_rgb8(0xef, 0x6f, 0x6f)))
                        .on_press(Message::CancelEdit).padding([1, 4]),
                ].spacing(2).into()
            } else {
                let display_text = if is_last && !state.asr.interim.is_empty() {
                    format!("{}\n— {} —", turn.user_text, state.asr.interim)
                } else { turn.user_text.clone() };
                // Selectable text (transparent text_input)
                text_input("", &display_text).size(fs * 0.9)
                    .style(move |_t, _s| text_input::Style {
                        background: Background::Color(Color::TRANSPARENT),
                        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
                        icon: Color::TRANSPARENT, placeholder: Color::TRANSPARENT,
                        value: body, selection: Color::from_rgba(0.3, 0.5, 0.9, 0.5),
                    }).into()
            };

            let mut card_rows: Vec<Element<Message>> = vec![user_line.into()];
            if !turn.reply.is_empty() {
                card_rows.push(text(format!("◀ {}", turn.reply))
                    .color(subtle).size(fs * 0.82).into());
            }

            // Highlight selected cards
            let is_selected = state.selected_turns.contains(&idx_u64);
            let sel_bg = if is_selected {
                Some(Background::Color(Color::from_rgba(0.2, 0.35, 0.6, 0.25)))
            } else { None };
            let card = container(column(card_rows).spacing(3))
                .padding([6, 8]).width(Length::Fill)
                .style(move |_theme| container::Style {
                    background: sel_bg,
                    border: Border { color: if is_selected { Color::from_rgba(0.3, 0.5, 0.9, 0.4) } else { Color::TRANSPARENT }, width: 1.0, radius: 6.0.into() },
                    ..Default::default()
                });
            let i = idx_u64;
            let n_selected = state.selected_turns.len();
            ContextMenu::new(
                mouse_area(card)
                    .on_press(Message::ToggleSelectTurn(i))
                    .on_right_press(Message::AsrContextMenu(i)),
                move || {
                    let mut menu = column![
                        button(text("📋 Copy to buffer").size(fs * 0.75)).on_press(Message::AsrContextMenu(i)).width(Length::Fill),
                        button(text("✏ Fix").size(fs * 0.75)).on_press(Message::FixTurn(i)).width(Length::Fill),
                        button(text("🔊 Play audio").size(fs * 0.75)).on_press(Message::PlayAudio(i)).width(Length::Fill),
                    ].spacing(2);
                    if n_selected > 1 {
                        menu = menu.push(
                            button(text(format!("📋 Copy selected ({n_selected})")).size(fs * 0.75))
                                .on_press(Message::CopySelectedTurns).width(Length::Fill)
                        );
                    }
                    menu.padding(4).into()
                },
            ).into()
        }).collect();
        section_rows.push(
            scrollable(column(items).spacing(4)).anchor_bottom()
                .height(Length::Fill).into()
        );
    } else if !state.section_collapsed[0] {
        section_rows.push(text("no messages yet").size(fs * 0.7).color(faint).into());
    }
    section_rows.push(divider(0, state.dragging_divider == Some(0)));

    // Clipboard Section
    section_rows.push(title_bar(1, "📋 Clipboard", state.section_collapsed[1], fs));
    if !state.section_collapsed[1] && !state.clipboard.is_empty() {
        let items: Vec<Element<Message>> = state.clipboard.iter().take(9).enumerate().map(|(i, s)| {
            let short = if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s.clone() };
            let idx = text(format!("{}", i + 1)).size(fs * 0.75).color(dim);
            let d = dim;
            let b = body;
            let txt: Element<Message> = text_input("", &short).size(fs * 0.85)
                .style(move |_t, _s| text_input::Style {
                    background: Background::Color(Color::TRANSPARENT),
                    border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
                    icon: Color::TRANSPARENT, placeholder: d,
                    value: b,
                    selection: Color::from_rgba(0.3, 0.5, 0.9, 0.5),
                }).into();
            container(row![idx, txt].spacing(6)).padding([2, 4]).width(Length::Fill).into()
        }).collect();
        section_rows.push(scrollable(column(items).spacing(1)).height(Length::Fill).into());
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
        section_rows.push(scrollable(column(items).spacing(0)).height(Length::Fill).into());
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
