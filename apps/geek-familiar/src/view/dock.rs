//! Dock buttons — drag handle, ASR status, etc.

use crate::model::{Message, StyleConfig};
use crate::PetApp;
use iced::widget::{container, mouse_area, text};
use iced::{alignment, mouse, Color, Element};

use super::style::pill_style;

pub fn drag_button(_fs: f32, style: StyleConfig) -> Element<'static, Message> {
    mouse_area(
        container(text("⠿").size(18.0).color(Color::from_rgb8(0xcc, 0xcc, 0xcc)))
            .width(32.0).height(28.0).align_x(alignment::Horizontal::Center).align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, false) }),
    ).on_press(Message::DragStarted).interaction(mouse::Interaction::Grab).into()
}

/// Three-state ASR dock button — icon only, same size/shape as drag button.
pub fn asr_dock_button(state: &PetApp, style: StyleConfig) -> Element<'static, Message> {
    let (icon, color) = match state.asr.status() {
        crate::model::AsrStatus::Enabled => ("●", Color::from_rgb8(0x4f, 0xef, 0x6f)),
        crate::model::AsrStatus::Disabled => ("◐", Color::from_rgb8(0xef, 0xcf, 0x4f)),
        crate::model::AsrStatus::Disconnected => ("○", Color::from_rgb8(0xef, 0x6f, 0x6f)),
    };
    mouse_area(
        container(text(icon).size(16.0).color(color))
            .width(32.0).height(28.0)
            .align_x(alignment::Horizontal::Center).align_y(alignment::Vertical::Center)
            .style({ let s = style.clone(); move |_theme| pill_style(&s, false) }),
    ).on_press(Message::ToggleRecording).interaction(mouse::Interaction::Pointer).into()
}
