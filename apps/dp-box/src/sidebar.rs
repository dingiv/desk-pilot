use iced::widget::{button, container, text, Column, Space};
use iced::{Element, Length};

use crate::tab::Tab;

pub fn sidebar(active_tab: &Tab) -> Element<'_, crate::app::Message> {
    let header = container(text("Desk Pilot Box").size(18).style(|_t| text::Style { color: Some(c_pri()) }))
        .padding(16u16);

    let nav = Column::with_children(vec![
        nav_button("Home", Tab::Home, active_tab),
        nav_button("App Store", Tab::AppStore, active_tab),
        sep(),
        nav_button("Geek Familiar", Tab::GeekFamiliar, active_tab),
    ]).spacing(2);

    let body = container(nav).padding([0u16, 6u16]);
    let bottom = container(text("v0.1.0").size(11).style(|_t| text::Style { color: Some(c_mut()) }))
        .padding([8u16, 12u16]);

    container(
        Column::with_children(vec![
            header.into(), body.into(),
            Space::new().height(Length::Fill).into(),
            bottom.into(),
        ]).width(Length::Fill).height(Length::Fill),
    )
    .width(180).height(Length::Fill).style(sidebar_bg).into()
}

fn nav_button(label: &str, tab: Tab, active: &Tab) -> Element<'static, crate::app::Message> {
    let is_sel = *active == tab;
    let txt = text(label.to_string()).size(13);
    let styled = if is_sel { txt.style(|_t| text::Style { color: Some(c_pri()) }) }
                else { txt.style(|_t| text::Style { color: Some(c_mut()) }) };
    let btn = button(styled)
        .on_press(crate::app::Message::TabSelected(tab))
        .padding([8u16, 12u16]).width(Length::Fill);
    if is_sel { btn.style(btn_active) } else { btn.style(btn_inactive) }.into()
}

fn sep() -> Element<'static, crate::app::Message> {
    container(Space::new().height(1)).padding([8u16, 12u16])
        .style(|_t| container::Style {
            border: iced::Border { color: c_border(), width: 1.0, radius: 0.0.into() },
            ..container::Style::default()
        }).width(Length::Fill).into()
}

// ── helpers ──
fn c_pri() -> iced::Color { iced::Color::from_rgb8(0xec, 0xec, 0xf0) }
fn c_mut() -> iced::Color { iced::Color::from_rgb8(0x88, 0x88, 0x99) }
fn c_border() -> iced::Color { iced::Color::from_rgba(0.25, 0.25, 0.30, 1.0) }

fn sidebar_bg(_t: &iced::Theme) -> container::Style {
    container::Style { background: Some(iced::Color::from_rgb8(0x16, 0x17, 0x20).into()), ..container::Style::default() }
}

fn btn_inactive(_t: &iced::Theme, _s: button::Status) -> button::Style {
    button::Style { background: None, text_color: c_mut(), border: iced::Border::default(), shadow: iced::Shadow::default(), snap: true }
}

fn btn_active(_t: &iced::Theme, _s: button::Status) -> button::Style {
    button::Style {
        background: Some(iced::Color::from_rgba(0.31, 0.56, 0.97, 0.15).into()),
        text_color: iced::Color::from_rgb8(0x4f, 0x8e, 0xf7),
        border: iced::Border { color: iced::Color::TRANSPARENT, width: 0.0, radius: 6.0.into() },
        shadow: iced::Shadow::default(), snap: true,
    }
}
