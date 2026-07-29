use iced::widget::{container, text, Column, Row, Space};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::theme;

pub fn view() -> Element<'static, Message> {
    let cs = container(
        Column::with_children(vec![
            text("Coming Soon").size(28).style(|_t| text::Style { color: Some(theme::MUTED) }).into(),
            Space::new().height(8).into(),
            text("Remote app installation will be available in a future update.").size(13)
                .style(|_t| text::Style { color: Some(theme::MUTED) }).into(),
        ]).spacing(4).align_x(Alignment::Center),
    ).style(theme::card_style).padding([40u16, 24u16]).width(Length::Fill).align_x(Alignment::Center);

    let sk = container(
        Row::with_children(vec![
            container(Space::new().width(48).height(48)).style(|_t| container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgba(0.31, 0.56, 0.97, 0.12))),
                border: iced::Border { radius: 8.0.into(), ..Default::default() },
                ..container::Style::default()
            }).into(),
            Column::with_children(vec![
                text("Example App").size(14).into(),
                text("cargo · v0.1.0").size(11).style(|_t| text::Style { color: Some(theme::MUTED) }).into(),
            ]).spacing(2).into(),
        ]).spacing(12).padding([12u16, 16u16]).align_y(Alignment::Center),
    ).style(theme::card_style).width(Length::Fill);

    container(
        Column::with_children(vec![
            text("App Store").size(22).into(),
            Space::new().height(16).into(),
            cs.into(),
            Space::new().height(20).into(),
            sk.into(),
        ]).spacing(4).padding([24u16, 24u16]),
    ).width(Length::Fill).height(Length::Fill).into()
}
