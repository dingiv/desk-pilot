use iced::widget::{button, container, text, Column, Row, Space};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::manifest::AppManifest;
use crate::process::AppStatus;
use crate::theme;

pub fn home_page<'a>(manifest: &'a AppManifest, status: &'a AppStatus) -> Element<'a, Message> {
    let (status_str, status_color) = match status {
        AppStatus::Running { pid } => (format!("Running (PID {pid})"), theme::SUCCESS),
        AppStatus::Stopped => ("Stopped".into(), theme::MUTED),
        AppStatus::Error(e) => (format!("Error: {e}"), theme::DANGER),
    };

    let status_badge = Row::with_children(vec![
        container(Space::new().width(8).height(8)).style(move |_t| container::Style {
            background: Some(iced::Background::Color(status_color)),
            border: iced::Border { radius: 4.0.into(), ..Default::default() },
            ..Default::default()
        }).into(),
        text(status_str).size(12).style(move |_t| text::Style { color: Some(status_color) }).into(),
    ]).spacing(6).align_y(Alignment::Center);

    let actions = Column::with_children(vec![
        button(text("Launch").size(13)).on_press(Message::LaunchApp)
            .style(btn_pri).padding([6u16, 16u16]).into(),
        button(text("Stop").size(13)).on_press(Message::StopApp)
            .style(btn_danger).padding([6u16, 16u16]).into(),
    ]).spacing(4);

    let info = Column::with_children(vec![
        text(&manifest.display_name).size(16).into(),
        text(&manifest.description).size(12).style(|_t| text::Style { color: Some(theme::MUTED) }).into(),
        status_badge.into(),
    ]).spacing(4);

    container(
        Row::with_children(vec![info.into(), Space::new().width(Length::Fill).into(), actions.into()])
            .spacing(12).padding([12u16, 16u16]).align_y(Alignment::Center),
    ).style(theme::card_style).width(Length::Fill).into()
}

pub fn view<'a>(manifest: &'a AppManifest, status: &'a AppStatus) -> Element<'a, Message> {
    container(
        Column::with_children(vec![
            text("Desk Pilot Box").size(22).into(),
            text("Manage your desk-pilot applications").size(13)
                .style(|_t| text::Style { color: Some(theme::MUTED) }).into(),
            Space::new().height(16).into(),
            text("Installed Applications").size(15).into(),
            Space::new().height(8).into(),
            home_page(manifest, status),
            Space::new().height(20).into(),
            text("Quick Actions").size(15).into(),
            Space::new().height(8).into(),
            Row::with_children(vec![
                button(text("Refresh Status").size(12)).on_press(Message::RefreshStatus)
                    .style(btn_sec).padding([6u16, 14u16]).into(),
            ]).spacing(8).into(),
        ]).spacing(4).padding([24u16, 24u16]),
    ).width(Length::Fill).height(Length::Fill).into()
}

fn btn_pri(_t: &iced::Theme, s: button::Status) -> button::Style { btn(s, theme::ACCENT) }
fn btn_danger(_t: &iced::Theme, s: button::Status) -> button::Style { btn(s, theme::DANGER) }
fn btn_sec(_t: &iced::Theme, s: button::Status) -> button::Style { btn(s, theme::MUTED) }

fn btn(s: button::Status, base: iced::Color) -> button::Style {
    let (bg, tc) = match s {
        button::Status::Hovered => (iced::Color::from_rgba(base.r, base.g, base.b, 0.2), base),
        button::Status::Pressed => (iced::Color::from_rgba(base.r, base.g, base.b, 0.3), base),
        _ => (iced::Color::from_rgba(base.r, base.g, base.b, 0.12), base),
    };
    button::Style {
        background: Some(iced::Background::Color(bg)), text_color: tc,
        border: iced::Border { color: iced::Color::from_rgba(base.r, base.g, base.b, 0.3), width: 1.0, radius: 6.0.into() },
        shadow: iced::Shadow::default(), snap: true,
    }
}
