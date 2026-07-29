use iced::widget::{button, container, text, text_input, Column, Row, Space};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::theme;

pub struct GeekFamiliarState {
    pub available_skins: Vec<String>,
    pub selected_skin: String,
    pub aura_connected: Option<bool>,
    pub config_text: String,
    pub config_saved: bool,
}

impl GeekFamiliarState {
    pub fn new() -> Self {
        let skins = list_skins();
        let sel = skins.first().cloned().unwrap_or_else(|| "default/idle.png".into());
        let loader = fs::loader!();
        let cfg = loader.read_str("CONF::familiar.yaml").unwrap_or_default();
        Self { available_skins: skins, selected_skin: sel, aura_connected: None, config_text: cfg, config_saved: true }
    }
}

pub fn view<'a>(state: &'a GeekFamiliarState) -> Element<'a, Message> {
    let header = Row::with_children(vec![
        text("Geek Familiar").size(22).into(),
        Space::new().width(Length::Fill).into(),
        button(text("Launch").size(13)).on_press(Message::GfLaunchToggled).style(btn_pri).padding([6u16, 16u16]).into(),
        button(text("Stop").size(13)).on_press(Message::GfLaunchToggled).style(btn_danger).padding([6u16, 16u16]).into(),
    ]).spacing(8).align_y(Alignment::Center);

    // skins
    let skin_title = text("Skin").size(14);
    let skin_label = text(&state.selected_skin).size(12).style(|_t| text::Style { color: Some(theme::MUTED) });
    let mut skin_btns = Column::new();
    for skin in &state.available_skins {
        let is_sel = skin == &state.selected_skin;
        let b = button(text(skin.clone()).size(12)).on_press(Message::GfSkinSelected(skin.clone())).padding([4u16, 10u16]);
        skin_btns = skin_btns.push(if is_sel { b.style(btn_sel) } else { b.style(btn_out) });
    }
    let skin_section = container(Column::with_children(vec![skin_title.into(), skin_label.into(), Space::new().height(6).into(), skin_btns.into()]).spacing(4))
        .style(theme::card_style).padding([12u16, 16u16]).width(Length::Fill);

    // aura
    let aura_status = match state.aura_connected {
        Some(true) => Row::with_children(vec![dot(theme::SUCCESS), text("Connected").size(12).into()]).spacing(6),
        Some(false) => Row::with_children(vec![dot(theme::DANGER), text("Disconnected").size(12).into()]).spacing(6),
        None => Row::with_children(vec![dot(theme::MUTED), text("Not checked").size(12).into()]).spacing(6),
    };
    let aura_section = container(Column::with_children(vec![
        text("Audio-Aura Connection").size(14).into(), aura_status.into(), Space::new().height(6).into(),
        button(text("Check Connection").size(12)).on_press(Message::GfAuraCheck).style(btn_out).padding([4u16, 12u16]).into(),
    ]).spacing(4)).style(theme::card_style).padding([12u16, 16u16]).width(Length::Fill);

    // config
    let save_fb = if state.config_saved {
        text("Saved").size(11).style(|_t| text::Style { color: Some(theme::SUCCESS) })
    } else {
        text("Modified").size(11).style(|_t| text::Style { color: Some(theme::MUTED) })
    };
    let config_section = container(Column::with_children(vec![
        text("Configuration").size(14).into(),
        text("Edit familiar.yaml").size(12).style(|_t| text::Style { color: Some(theme::MUTED) }).into(),
        Space::new().height(6).into(),
        text_input("", &state.config_text).on_input(Message::GfConfigTextChanged).size(12).padding([8u16, 10u16]).into(),
        Space::new().height(6).into(),
        Row::with_children(vec![
            button(text("Save Config").size(12)).on_press(Message::GfConfigSaved).style(btn_pri).padding([6u16, 14u16]).into(),
            save_fb.into(),
        ]).spacing(8).align_y(Alignment::Center).into(),
    ]).spacing(4)).style(theme::card_style).padding([12u16, 16u16]).width(Length::Fill);

    container(
        Column::with_children(vec![
            header.into(), Space::new().height(16).into(),
            skin_section.into(), Space::new().height(12).into(),
            aura_section.into(), Space::new().height(12).into(),
            config_section.into(),
        ]).spacing(4).padding([24u16, 24u16]),
    ).width(Length::Fill).height(Length::Fill).into()
}

fn list_skins() -> Vec<String> {
    let ws = crate::manifest::detect_workspace_root().join("apps/geek-familiar/assets/skins");
    if !ws.exists() { return vec!["default/idle.png".into()]; }
    let mut v = Vec::new();
    if let Ok(e) = std::fs::read_dir(&ws) {
        for en in e.flatten() {
            if en.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let dn = en.file_name().to_string_lossy().to_string();
                if en.path().join("idle.png").exists() { v.push(format!("{dn}/idle.png")); }
            }
        }
    }
    if v.is_empty() { v.push("default/idle.png".into()); }
    v.sort(); v
}

fn dot(c: iced::Color) -> Element<'static, Message> {
    container(Space::new().width(8).height(8)).style(move |_t| container::Style {
        background: Some(iced::Background::Color(c)),
        border: iced::Border { radius: 4.0.into(), ..Default::default() },
        ..container::Style::default()
    }).into()
}

fn btn_pri(_t: &iced::Theme, s: button::Status) -> button::Style { b(s, theme::ACCENT) }
fn btn_danger(_t: &iced::Theme, s: button::Status) -> button::Style { b(s, theme::DANGER) }
fn btn_out(_t: &iced::Theme, s: button::Status) -> button::Style { b(s, theme::MUTED) }

fn btn_sel(_t: &iced::Theme, _s: button::Status) -> button::Style {
    button::Style { background: Some(iced::Background::Color(theme::ACCENT)), text_color: iced::Color::WHITE,
        border: iced::Border { radius: 6.0.into(), ..Default::default() }, shadow: iced::Shadow::default(), snap: true }
}

fn b(s: button::Status, base: iced::Color) -> button::Style {
    let (bg, tc) = match s {
        button::Status::Hovered => (iced::Color::from_rgba(base.r, base.g, base.b, 0.2), base),
        button::Status::Pressed => (iced::Color::from_rgba(base.r, base.g, base.b, 0.3), base),
        _ => (iced::Color::from_rgba(base.r, base.g, base.b, 0.12), base),
    };
    button::Style { background: Some(iced::Background::Color(bg)), text_color: tc,
        border: iced::Border { color: iced::Color::from_rgba(base.r, base.g, base.b, 0.3), width: 1.0, radius: 6.0.into() },
        shadow: iced::Shadow::default(), snap: true }
}
