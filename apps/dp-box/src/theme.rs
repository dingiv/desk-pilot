use iced::Color;

pub fn manager_theme() -> iced::Theme {
    iced::Theme::custom(
        "desk-pilot-dark",
        iced::theme::Palette {
            background: Color::from_rgb8(0x1a, 0x1b, 0x26),
            text: Color::from_rgb8(0xec, 0xec, 0xf0),
            primary: Color::from_rgb8(0x4f, 0x8e, 0xf7),
            success: Color::from_rgb8(0x4f, 0xef, 0x6f),
            danger: Color::from_rgb8(0xef, 0x4f, 0x4f),
            ..iced::Theme::Dark.palette()
        },
    )
}

pub const SURFACE: Color = Color::from_rgb8(0x24, 0x25, 0x33);
pub const BORDER: Color = Color::from_rgba(0.25, 0.25, 0.30, 0.6);
pub const MUTED: Color = Color::from_rgb8(0x88, 0x88, 0x99);
pub const ACCENT: Color = Color::from_rgb8(0x4f, 0x8e, 0xf7);
pub const SUCCESS: Color = Color::from_rgb8(0x4f, 0xef, 0x6f);
pub const DANGER: Color = Color::from_rgb8(0xef, 0x4f, 0x4f);

pub fn card_style(_theme: &iced::Theme) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(iced::Background::Color(SURFACE)),
        border: iced::Border {
            color: BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}
