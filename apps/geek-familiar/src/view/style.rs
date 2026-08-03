//! Styling helpers — colours, cards, pills, text.

use crate::model::StyleConfig;
use iced::{Background, Border, Color, Degrees, Gradient, Shadow, Vector};
use iced_core::gradient::Linear;

use iced::widget::container;

// ── Text colour helpers — each reads its own config field ────────────────────

pub fn text_color(style: &StyleConfig) -> Color {
    let [r, g, b, a] = style.text_color;
    Color::from_rgba(r, g, b, a)
}

pub fn text_dim(style: &StyleConfig) -> Color {
    let [r, g, b, a] = style.text_dim;
    Color::from_rgba(r, g, b, a)
}

pub fn text_faint(style: &StyleConfig) -> Color {
    let [r, g, b, a] = style.text_faint;
    Color::from_rgba(r, g, b, a)
}

pub fn text_subtle(style: &StyleConfig) -> Color {
    let [r, g, b, a] = style.text_subtle;
    Color::from_rgba(r, g, b, a)
}

// ── Container styles ─────────────────────────────────────────────────────────

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
